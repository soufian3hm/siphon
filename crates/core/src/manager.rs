//! The download manager: the queue, the persistent list, and the one API the
//! user interface and the browser bridge talk to.
//!
//! Every public method is synchronous and returns immediately; the work runs
//! on the manager's own runtime. Callers read state through
//! [`Manager::snapshot`] and are told when to look again through the `wake`
//! callback given at construction.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::checksum::{self, HashAlgorithm};
use crate::disk::write_atomic;
use crate::error::{Error, ErrorClass, Result};
use crate::filename;
use crate::finishing::{self, Origin};
use crate::grab::{self, GrabOptions, GrabProgress, LinkBatch};
use crate::http::HttpSource;
use crate::net::{NetConfig, build_client};
use crate::organize::{self, SiteRule};
use crate::proxy_bridge::ProxyBridge;
use crate::rate::{Limiter, SpeedMeter, eta_secs};
use crate::schedule::{Crossing, LocalTime, WhenDone};
use crate::secrets::{self, Secrets};
use crate::settings::Settings;
use crate::source::{Probe, Source};
use crate::transfer::{self, Outcome, Plan, Progress, Tuning};
use crate::types::{Category, Destination, DownloadId, Protocol, Request, SegmentSnapshot, Status, unix_now};
use crate::zip_peek::{self, ArchiveListing};

const TICK: Duration = Duration::from_millis(250);
const SAVE_EVERY: Duration = Duration::from_secs(2);
/// Automatic retries after a network failure, before waiting for the user.
const MAX_AUTO_RETRIES: u32 = 6;
/// How long a download waits for the browser to hand over its fresh link.
const AWAIT_FRESH_LINK: Duration = Duration::from_secs(15 * 60);

/// One entry of the download list, as stored on disk.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub id: DownloadId,
    pub request: Request,
    pub protocol: Protocol,
    pub dir: PathBuf,
    pub file_name: String,
    /// The name is final: chosen by the user, or already used on disk.
    #[serde(default)]
    pub name_locked: bool,
    pub total: Option<u64>,
    pub downloaded: u64,
    pub status: Status,
    /// `None` until the server has been asked.
    #[serde(default)]
    pub resumable: Option<bool>,
    /// Per-download connection count; `None` follows the setting.
    #[serde(default)]
    pub connections: Option<usize>,
    #[serde(default)]
    pub content_type: Option<String>,
    pub created_at: u64,
    #[serde(default)]
    pub finished_at: Option<u64>,
    /// A line of extra detail from the source, e.g. the stream quality.
    #[serde(default)]
    pub note: Option<String>,
    /// Started on the user's behalf while they were still being asked. It
    /// stays out of the list until they say yes, and leaves no trace if they
    /// say no.
    #[serde(default)]
    pub provisional: bool,
    /// The folder was picked from a guess at the file's name and may be
    /// corrected once the server gives the real one.
    #[serde(default)]
    pub auto_dir: bool,
    /// Where the file belongs, when that is not where it is being written.
    /// It moves there the moment the transfer lets go of it.
    #[serde(default)]
    pub deliver: Option<Destination>,
    /// Started by the queue, so the queue's stop time applies to it.
    #[serde(default)]
    pub queue_run: bool,
    /// For a video page: the chosen quality (a height, or sound only).
    #[serde(default)]
    pub variant: Option<String>,
}

impl Record {
    pub fn path(&self) -> PathBuf {
        self.dir.join(&self.file_name)
    }
}

/// A request to start a download.
#[derive(Clone, Debug, Default)]
pub struct NewDownload {
    pub request: Request,
    /// `None` uses the default download folder.
    pub dir: Option<PathBuf>,
    /// `None` lets the server name the file.
    pub file_name: Option<String>,
    pub connections: Option<usize>,
    /// Add to the list without starting.
    pub paused: bool,
    /// Set aside for the queue: it starts when the queue does.
    pub later: bool,
    /// What is already known about the file, so the list shows it at once.
    pub total: Option<u64>,
    pub content_type: Option<String>,
    /// Start now, but keep it out of the list until [`Manager::confirm`].
    pub provisional: bool,
    /// For a video page: the quality to fetch. `None` follows the setting.
    pub variant: Option<String>,
}

/// One quality a video page offers, as shown to the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VariantInfo {
    pub id: String,
    /// `"1080p60"`.
    pub label: String,
    /// `"MP4 · H.264"`.
    pub detail: String,
    pub size: Option<u64>,
    /// Extension of the finished file.
    pub container: String,
}

impl From<&crate::extractor::Variant> for VariantInfo {
    fn from(variant: &crate::extractor::Variant) -> Self {
        Self { id: variant.id.clone(), label: variant.label.clone(), detail: variant.detail.clone(), size: variant.size, container: variant.container.to_owned() }
    }
}

/// What the user decided about a download that was started before they
/// were asked.
#[derive(Clone, Debug, Default)]
pub struct Confirmation {
    /// `None` keeps the folder it is already in.
    pub dir: Option<PathBuf>,
    /// `None` keeps the name the server gives it.
    pub file_name: Option<String>,
    pub connections: Option<usize>,
    /// Keep it for the queue instead of carrying on now.
    pub later: bool,
}

/// An entry of the list that has the same address as a new request.
#[derive(Clone, Debug, PartialEq)]
pub struct Duplicate {
    pub id: DownloadId,
    pub file_name: String,
    pub status: Status,
    pub path: PathBuf,
    /// The finished file is still where it was saved.
    pub file_exists: bool,
}

/// The queue of downloads set aside for later.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueState {
    pub running: bool,
    /// Downloads waiting for the queue to start.
    pub waiting: usize,
    /// Downloads the running queue still has to finish.
    pub remaining: usize,
    /// Minutes until the scheduler starts the queue by itself.
    pub next_start_in: Option<u32>,
}

impl NewDownload {
    pub fn new(request: Request) -> Self {
        Self { request, ..Default::default() }
    }
}

/// A link handed over by the browser.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Capture {
    pub request: Request,
    /// The name the browser would have used.
    pub suggested_name: Option<String>,
    pub size: Option<u64>,
    pub mime: Option<String>,
    pub page_title: Option<String>,
    /// True for a media stream sniffed from a page rather than a download
    /// the user triggered.
    pub media: bool,
}

/// What a server said about a link, for the confirmation dialog.
#[derive(Clone, Debug, PartialEq)]
pub struct ProbeInfo {
    pub file_name: String,
    pub total: Option<u64>,
    pub resumable: bool,
    pub content_type: Option<String>,
    pub final_url: String,
    pub protocol: Protocol,
    pub note: Option<String>,
    /// For a video page: the qualities on offer, best first, and the one
    /// that would be fetched.
    pub variants: Vec<VariantInfo>,
    pub variant: Option<String>,
}

/// Things the user interface may want to react to.
#[derive(Clone, Debug)]
pub enum Event {
    /// The browser sent a link and the user wants to confirm it first.
    /// `provisional` is the download already fetching it in the background,
    /// when instant start is on.
    Captured { capture: Capture, provisional: Option<DownloadId> },
    /// A captured link is already being downloaded; no second copy was made.
    AlreadyDownloading { id: DownloadId, name: String },
    /// The browser sent the links of a page for the user to choose from.
    Links(LinkBatch),
    QueueStarted { count: usize, by_schedule: bool },
    /// The queue has nothing left to do.
    QueueFinished { when_done: WhenDone, completed: usize, failed: usize },
    /// Something worth a line of text.
    Notice { text: String, bad: bool },
    /// A captured link was started without asking.
    AutoStarted { id: DownloadId, name: String },
    /// A captured link turned out to be a fresh URL for a stalled download.
    AutoRefreshed { id: DownloadId, name: String },
    Completed { id: DownloadId, name: String, path: PathBuf },
    Failed { id: DownloadId, name: String, message: String },
    Expired { id: DownloadId, name: String },
    /// Another launch of the application asked this one to come forward.
    Raise,
}

/// The result of background work, filled in when it is ready.
pub struct Ticket<T>(Arc<Mutex<Option<T>>>);

impl<T> Clone for Ticket<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> Default for Ticket<T> {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }
}

impl<T> Ticket<T> {
    /// Takes the result if it has arrived.
    pub fn take(&self) -> Option<T> {
        self.0.lock().take()
    }

    pub fn is_ready(&self) -> bool {
        self.0.lock().is_some()
    }

    fn fill(&self, value: T) {
        *self.0.lock() = Some(value);
    }
}

/// A checksum being computed in the background. Dropping it stops the work.
pub struct HashJob {
    pub algorithm: HashAlgorithm,
    total: u64,
    done: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
    result: Ticket<std::result::Result<String, String>>,
}

impl HashJob {
    /// Share of the file read so far, in `0..=1`.
    pub fn fraction(&self) -> f32 {
        if self.total == 0 { 1.0 } else { (self.done.load(Ordering::Relaxed) as f64 / self.total as f64).min(1.0) as f32 }
    }

    /// The digest in lower-case hexadecimal, once it is ready.
    pub fn take(&self) -> Option<std::result::Result<String, String>> {
        self.result.take()
    }
}

impl Drop for HashJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// A page (or site) being explored for links. Dropping it stops the work.
pub struct GrabJob {
    pub progress: Arc<GrabProgress>,
    cancel: CancellationToken,
    result: Ticket<Result<LinkBatch>>,
}

impl GrabJob {
    pub fn take(&self) -> Option<Result<LinkBatch>> {
        self.result.take()
    }
}

impl Drop for GrabJob {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// A row of the download list, ready to draw.
#[derive(Clone, Debug, PartialEq)]
pub struct DownloadView {
    pub id: DownloadId,
    pub file_name: String,
    pub dir: PathBuf,
    pub url: String,
    pub referer: Option<String>,
    pub protocol: Protocol,
    pub category: Category,
    pub status: Status,
    pub total: Option<u64>,
    pub downloaded: u64,
    /// Completed share in `0..=1`, when it can be known.
    pub fraction: Option<f32>,
    pub bytes_per_sec: f64,
    pub eta_secs: Option<u64>,
    pub connections: u32,
    pub resumable: Option<bool>,
    pub content_type: Option<String>,
    pub created_at: u64,
    pub finished_at: Option<u64>,
    pub note: Option<String>,
    /// Seconds until the next automatic retry, for a failed download that
    /// will try again by itself.
    pub retry_in: Option<u64>,
    /// Where the file is being written for now, when the user has since
    /// chosen another place for it.
    pub staged_at: Option<PathBuf>,
    /// Started by the queue.
    pub queue_run: bool,
    pub provisional: bool,
    /// For a video page: the chosen quality.
    pub variant: Option<String>,
}

impl DownloadView {
    /// Where the file is, or will be once it is finished.
    pub fn path(&self) -> PathBuf {
        self.dir.join(&self.file_name)
    }

    /// Where its bytes are on disk right now.
    pub fn disk_path(&self) -> PathBuf {
        self.staged_at.clone().unwrap_or_else(|| self.path())
    }
}

/// The parts of a download that are only worth computing for the one the
/// user is looking at.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Details {
    pub segments: Vec<SegmentSnapshot>,
    pub speed_history: Vec<f32>,
}

/// Why a running task was asked to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Intent {
    Run,
    Pause,
    Remove { trash_file: bool },
    Restart,
    Shutdown,
    /// Stop and wait for the queue.
    Hold,
}

struct Item {
    record: Record,
    progress: Arc<Progress>,
    /// Present while a task is running for this download.
    cancel: Option<CancellationToken>,
    intent: Intent,
    meter: SpeedMeter,
    retries: u32,
    retry_at: Option<Instant>,
    /// The server's `Last-Modified`, kept for the finished file's date.
    last_modified: Option<String>,
    /// For a video page: the qualities it was last found to offer.
    variants: Vec<VariantInfo>,
}

impl Item {
    fn new(record: Record) -> Self {
        Self {
            record,
            progress: Arc::new(Progress::default()),
            cancel: None,
            intent: Intent::Run,
            meter: SpeedMeter::default(),
            retries: 0,
            retry_at: None,
            last_modified: None,
            variants: Vec::new(),
        }
    }
}

struct State {
    settings: Settings,
    /// Newest first.
    order: Vec<DownloadId>,
    items: HashMap<DownloadId, Item>,
    next_id: u64,
    /// The queue is working through the downloads that were set aside.
    queue_running: bool,
    /// A stalled download whose fresh link the browser is expected to send.
    awaiting: Option<(DownloadId, Instant)>,
}

#[derive(Serialize, Deserialize)]
struct Library {
    next_id: u64,
    downloads: Vec<Record>,
}

struct Inner {
    data_dir: PathBuf,
    runtime: tokio::runtime::Handle,
    state: Mutex<State>,
    limiter: Arc<Limiter>,
    client: RwLock<Result<reqwest::Client>>,
    /// Alive while the proxy needs NTLM, Negotiate or Kerberos.
    bridge: Mutex<Option<ProxyBridge>>,
    secrets: Arc<Secrets>,
    /// Replaces the video helper's location in tests.
    resolver: Mutex<Option<PathBuf>>,
    /// Called when something wants the user's eyes: a captured link, a page
    /// of links, a request to come forward. Unlike `wake`, it must work
    /// while the window is hidden and nothing is being drawn.
    attention: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// The helper that holds off idle sleep while downloads run.
    awake: Mutex<Option<std::process::Child>>,
    wake: Box<dyn Fn() + Send + Sync>,
    events: Mutex<Vec<Event>>,
    dirty: AtomicBool,
    closing: AtomicBool,
    /// Replaces the shipping tuning in tests.
    tuning: Mutex<Option<Tuning>>,
}

/// The download manager. Dropping it stops everything after a final save.
pub struct Manager {
    inner: Arc<Inner>,
    runtime: Option<tokio::runtime::Runtime>,
}

impl Manager {
    /// Opens the library in `data_dir`, creating it if needed.
    ///
    /// `wake` is called from background threads whenever something visible
    /// changed; the interface uses it to schedule a repaint.
    pub fn open(data_dir: &Path, wake: impl Fn() + Send + Sync + 'static) -> std::io::Result<Self> {
        Self::open_with(data_dir, Secrets::system(), wake)
    }

    /// Like [`Manager::open`], with the password store chosen by the caller.
    pub fn open_with(data_dir: &Path, secrets: Secrets, wake: impl Fn() + Send + Sync + 'static) -> std::io::Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("siphon-engine")
            .build()?;

        let settings = Settings::load(&data_dir.join("settings.json"));
        let library = std::fs::read(data_dir.join("downloads.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Library>(&bytes).ok())
            .unwrap_or(Library { next_id: 1, downloads: Vec::new() });

        let mut order = Vec::with_capacity(library.downloads.len());
        let mut items = HashMap::with_capacity(library.downloads.len());
        let mut next_id = library.next_id.max(1);
        for mut record in library.downloads {
            next_id = next_id.max(record.id.0 + 1);
            if record.provisional {
                // The application closed with the question still on screen.
                // It was never accepted, so nothing of it is kept.
                discard_files(&record, false);
                continue;
            }
            if let Some(dest) = record.deliver.take() {
                // Nothing is writing now: the move that was waiting can happen.
                if let Ok(placed) = relocate(&record.path(), &dest, &[]) {
                    record.dir = placed.dir;
                    record.file_name = placed.file_name;
                    record.name_locked = true;
                }
            }
            record.queue_run = false;
            // Whatever was in flight when the process ended either continues
            // or waits for the user, depending on the setting.
            if record.status.is_active() || record.status == Status::Queued {
                record.status = if settings.resume_on_launch { Status::Queued } else { Status::Paused };
            }
            if let Some((downloaded, _)) = transfer::resumable_bytes(&record.path())
                && !record.status.is_finished()
            {
                record.downloaded = downloaded;
            }
            order.push(record.id);
            items.insert(record.id, Item::new(record));
        }

        let limiter = Arc::new(Limiter::new(settings.effective_speed_limit()));
        let (client, bridge) = connect(runtime.handle(), &settings.net);
        let inner = Arc::new(Inner {
            data_dir: data_dir.to_owned(),
            runtime: runtime.handle().clone(),
            state: Mutex::new(State { settings, order, items, next_id, queue_running: false, awaiting: None }),
            limiter,
            client: RwLock::new(client),
            bridge: Mutex::new(bridge),
            secrets: Arc::new(secrets),
            resolver: Mutex::new(None),
            attention: Mutex::new(None),
            awake: Mutex::new(None),
            wake: Box::new(wake),
            events: Mutex::new(Vec::new()),
            dirty: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            tuning: Mutex::new(None),
        });

        runtime.spawn(ticker(inner.clone()));
        // Opened inside the scheduled window: the queue should be running.
        if inner.state.lock().settings.schedule.in_window(LocalTime::now()) {
            inner.start_queue(true);
        }
        inner.pump();
        Ok(Self { inner, runtime: Some(runtime) })
    }

    /// Sets what happens when the user's attention is wanted. The interface
    /// uses it to bring its window forward by a route that does not depend
    /// on the window currently being drawn.
    pub fn on_attention(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.inner.attention.lock() = Some(Box::new(hook));
    }

    /// Handle to the engine's runtime, for callers with their own async work.
    pub fn runtime(&self) -> &tokio::runtime::Handle {
        &self.inner.runtime
    }

    pub fn data_dir(&self) -> &Path {
        &self.inner.data_dir
    }

    pub fn settings(&self) -> Settings {
        self.inner.state.lock().settings.clone()
    }

    /// Replaces the settings, applies them to running work and saves them.
    pub fn set_settings(&self, settings: Settings) -> Result<()> {
        let settings = settings.sanitized();
        let net_changed = {
            let mut state = self.inner.state.lock();
            let changed = state.settings.net != settings.net;
            state.settings = settings.clone();
            changed
        };
        self.inner.limiter.set_rate(settings.effective_speed_limit());
        let mut outcome = Ok(());
        if net_changed {
            let (client, bridge) = connect(&self.inner.runtime, &settings.net);
            if let Err(e) = &client {
                outcome = Err(e.clone());
            }
            *self.inner.client.write() = client;
            // Replacing the bridge stops the previous one.
            *self.inner.bridge.lock() = bridge;
        }
        settings
            .save(&self.inner.data_dir.join("settings.json"))
            .map_err(|e| Error::Disk(format!("could not save settings: {e}")))?;
        // A raised concurrency limit can start queued downloads right away.
        self.inner.pump();
        outcome
    }

    /// Adds a download and, unless it is added paused, queues it.
    pub fn add(&self, new: NewDownload) -> DownloadId {
        let id = self.inner.insert(new);
        self.inner.pump();
        (self.inner.wake)();
        id
    }

    /// Adds several downloads in one go. They start in the order given.
    pub fn add_many(&self, downloads: Vec<NewDownload>) -> Vec<DownloadId> {
        let ids: Vec<DownloadId> = downloads.into_iter().map(|new| self.inner.insert(new)).collect();
        self.inner.pump();
        (self.inner.wake)();
        ids
    }

    /// Asks the server about a link without downloading it.
    pub fn probe(&self, request: Request) -> Ticket<Result<ProbeInfo>> {
        let ticket = Ticket::default();
        let inner = self.inner.clone();
        let filled = ticket.clone();
        self.inner.runtime.spawn(async move {
            filled.fill(inner.probe(&request).await);
            (inner.wake)();
        });
        ticket
    }

    /// Stops a download, keeping what it has.
    pub fn pause(&self, id: DownloadId) {
        {
            let mut state = self.inner.state.lock();
            let Some(item) = state.items.get_mut(&id) else { return };
            // Set aside for the queue is already not running.
            if item.record.status == Status::Scheduled {
                return;
            }
            item.retry_at = None;
            // Stopped by hand: the queue no longer speaks for it.
            item.record.queue_run = false;
            match &item.cancel {
                Some(cancel) => {
                    item.intent = Intent::Pause;
                    cancel.cancel();
                }
                None if item.record.status == Status::Queued || item.record.status.can_resume() => {
                    // A failed download waiting for its automatic retry is
                    // paused too: the user said stop.
                    if !matches!(item.record.status, Status::Failed { .. }) {
                        item.record.status = Status::Paused;
                    }
                }
                None => {}
            }
        }
        self.inner.touch();
    }

    /// Queues a stopped download again.
    pub fn resume(&self, id: DownloadId) {
        {
            let mut state = self.inner.state.lock();
            let Some(item) = state.items.get_mut(&id) else { return };
            if item.cancel.is_some() || item.record.status.is_finished() || item.record.status == Status::Queued {
                return;
            }
            item.retries = 0;
            item.retry_at = None;
            item.intent = Intent::Run;
            item.record.status = Status::Queued;
        }
        self.inner.touch();
        self.inner.pump();
    }

    /// Tries a stalled or failed download again with the request it already
    /// has, for instance once a login for its site has been saved.
    pub fn retry(&self, id: DownloadId) {
        {
            let mut state = self.inner.state.lock();
            let Some(item) = state.items.get_mut(&id) else { return };
            if item.cancel.is_some() || !matches!(item.record.status, Status::NeedsRefresh { .. } | Status::Failed { .. }) {
                return;
            }
            item.retries = 0;
            item.retry_at = None;
            item.intent = Intent::Run;
            item.record.status = Status::Queued;
        }
        self.inner.touch();
        self.inner.pump();
    }

    pub fn pause_all(&self) {
        let ids: Vec<DownloadId> = {
            let state = self.inner.state.lock();
            state.order.iter().copied().filter(|id| state.items[id].record.status.can_pause() && !state.items[id].record.provisional).collect()
        };
        for id in ids {
            self.pause(id);
        }
    }

    pub fn resume_all(&self) {
        let ids: Vec<DownloadId> = {
            let state = self.inner.state.lock();
            // Oldest first, so the queue keeps the order things were added in.
            // What was set aside for the queue stays set aside.
            state
                .order
                .iter()
                .rev()
                .copied()
                .filter(|id| {
                    let record = &state.items[id].record;
                    record.status.can_resume() && record.status != Status::Scheduled && !record.provisional
                })
                .collect()
        };
        for id in ids {
            self.resume(id);
        }
    }

    /// Removes a download from the list. Partial data is always deleted; a
    /// finished file is kept unless `trash_file` is set, in which case it is
    /// moved to the system trash.
    pub fn remove(&self, id: DownloadId, trash_file: bool) {
        let finished = {
            let mut state = self.inner.state.lock();
            let Some(item) = state.items.get_mut(&id) else { return };
            if let Some(cancel) = &item.cancel {
                item.intent = Intent::Remove { trash_file };
                cancel.cancel();
                None
            } else {
                let item = state.items.remove(&id).expect("checked above");
                state.order.retain(|other| *other != id);
                Some(item.record)
            }
        };
        if let Some(record) = finished {
            discard_files(&record, trash_file);
        }
        self.inner.touch();
        self.inner.pump();
    }

    /// Throws away what was downloaded and starts over from the first byte.
    pub fn restart(&self, id: DownloadId) {
        {
            let mut state = self.inner.state.lock();
            let Some(item) = state.items.get_mut(&id) else { return };
            if let Some(cancel) = &item.cancel {
                item.intent = Intent::Restart;
                cancel.cancel();
            } else {
                reset_for_restart(item);
            }
        }
        self.inner.touch();
        self.inner.pump();
    }

    /// Replaces the link of a stalled download.
    ///
    /// The new link is asked for its size first; it is accepted only when it
    /// describes the same file. The download then continues from its saved
    /// byte map with the new URL, cookies and headers.
    pub fn refresh_link(&self, id: DownloadId, request: Request) -> Ticket<Result<()>> {
        let ticket = Ticket::default();
        let inner = self.inner.clone();
        let filled = ticket.clone();
        self.inner.runtime.spawn(async move {
            filled.fill(inner.refresh_link(id, request).await);
            (inner.wake)();
        });
        ticket
    }

    /// Moves a stopped or finished download (data file, partial file and
    /// sidecar alike) to another folder.
    pub fn move_to(&self, id: DownloadId, dir: &Path) -> Result<()> {
        let mut state = self.inner.state.lock();
        let reserved = reserved_paths(&state, Some(id));
        let item = state.items.get_mut(&id).ok_or_else(|| Error::Protocol("no such download".into()))?;
        if item.cancel.is_some() {
            return Err(Error::Unsupported("Pause the download before moving it.".into()));
        }
        if item.record.dir == dir {
            return Ok(());
        }
        let wanted = Destination { dir: dir.to_owned(), file_name: item.record.file_name.clone() };
        let placed = relocate(&item.record.path(), &wanted, &reserved).map_err(|e| Error::Disk(format!("could not move to {}: {e}", dir.display())))?;
        item.record.dir = placed.dir;
        item.record.file_name = placed.file_name;
        item.record.name_locked = true;
        item.record.auto_dir = false;
        drop(state);
        self.inner.touch();
        Ok(())
    }

    /// Renames a stopped or finished download.
    pub fn rename(&self, id: DownloadId, name: &str) -> Result<()> {
        let name = filename::sanitize(name);
        let mut state = self.inner.state.lock();
        let reserved = reserved_paths(&state, Some(id));
        let item = state.items.get_mut(&id).ok_or_else(|| Error::Protocol("no such download".into()))?;
        if item.cancel.is_some() {
            return Err(Error::Unsupported("Pause the download before renaming it.".into()));
        }
        if item.record.file_name == name {
            return Ok(());
        }
        let old = item.record.path();
        let new = item.record.dir.join(&name);
        if new.exists() || filename::part_path(&new).exists() || reserved.contains(&new) {
            return Err(Error::Disk(format!("\"{name}\" already exists in that folder")));
        }
        move_set(&old, &new).map_err(|e| Error::Disk(format!("could not rename: {e}")))?;
        item.record.file_name = name;
        item.record.name_locked = true;
        drop(state);
        self.inner.touch();
        Ok(())
    }

    /// Removes every finished download from the list, keeping the files.
    pub fn clear_completed(&self) -> usize {
        let removed = {
            let mut state = self.inner.state.lock();
            let done: Vec<DownloadId> = state
                .order
                .iter()
                .copied()
                .filter(|id| state.items[id].record.status.is_finished() && !state.items[id].record.provisional)
                .collect();
            for id in &done {
                state.items.remove(id);
            }
            state.order.retain(|id| !done.contains(id));
            done.len()
        };
        self.inner.touch();
        removed
    }

    /// The whole list, newest first.
    pub fn snapshot(&self) -> Vec<DownloadView> {
        let state = self.inner.state.lock();
        let now = Instant::now();
        state
            .order
            .iter()
            .filter_map(|id| state.items.get(id))
            .filter(|item| !item.record.provisional)
            .map(|item| view_of(item, now, &state.settings))
            .collect()
    }

    /// One download, including one that is still provisional.
    pub fn get(&self, id: DownloadId) -> Option<DownloadView> {
        let state = self.inner.state.lock();
        state.items.get(&id).map(|item| view_of(item, Instant::now(), &state.settings))
    }

    pub fn details(&self, id: DownloadId) -> Option<Details> {
        let state = self.inner.state.lock();
        let item = state.items.get(&id)?;
        let mut segments = item.progress.segments();
        if segments.is_empty() && !item.record.status.is_finished() {
            // Not running: show the byte map as it sits on disk.
            if let Some(meta) = crate::disk::ResumeMeta::load(&filename::meta_path(&item.record.path())) {
                segments = meta.segments;
            }
        }
        Some(Details { segments, speed_history: item.meter.history() })
    }

    /// A stalled download that a captured link of this size and name would
    /// plausibly continue.
    pub fn refresh_candidate(&self, total: Option<u64>, file_name: Option<&str>) -> Option<DownloadId> {
        let state = self.inner.state.lock();
        refresh_candidate(&state, total, file_name)
    }

    /// An unfinished download of exactly this URL, if there is one.
    pub fn find_by_url(&self, url: &str) -> Option<DownloadId> {
        let url = identity(url);
        let state = self.inner.state.lock();
        state
            .order
            .iter()
            .copied()
            .find(|id| {
                let record = &state.items[id].record;
                record.request.url == url && !record.status.is_finished() && !record.provisional
            })
    }

    /// The newest entry of the list with exactly this address, finished or not.
    pub fn duplicate_of(&self, url: &str) -> Option<Duplicate> {
        let url = identity(url);
        let state = self.inner.state.lock();
        state.order.iter().filter_map(|id| state.items.get(id)).find(|item| !item.record.provisional && item.record.request.url == url).map(|item| {
            let path = item.record.path();
            Duplicate {
                id: item.record.id,
                file_name: item.record.file_name.clone(),
                status: item.record.status.clone(),
                file_exists: item.record.status.is_finished() && path.is_file(),
                path,
            }
        })
    }

    /// Takes the events that accumulated since the last call.
    pub fn drain_events(&self) -> Vec<Event> {
        std::mem::take(&mut *self.inner.events.lock())
    }

    /// Entry point for links arriving from the browser.
    ///
    /// With "save automatically" on, the link starts at once (or revives a
    /// stalled download it matches). Otherwise it is queued as an event for
    /// the interface to confirm.
    pub fn capture(&self, capture: Capture) {
        let (awaiting, auto) = {
            let mut state = self.inner.state.lock();
            let awaiting = match state.awaiting {
                Some((id, since)) if since.elapsed() < AWAIT_FRESH_LINK && state.items.get(&id).is_some_and(|i| !i.record.status.is_finished() && i.cancel.is_none()) => Some(id),
                Some(_) => {
                    state.awaiting = None;
                    None
                }
                None => None,
            };
            (awaiting, state.settings.save_automatically)
        };
        if awaiting.is_none() && !auto {
            self.inner.ask_about(capture);
            return;
        }
        let inner = self.inner.clone();
        self.inner.runtime.spawn(async move {
            // A download waiting for its fresh link gets first refusal.
            if let Some(id) = awaiting
                && inner.refresh_link(id, capture.request.clone()).await.is_ok()
            {
                inner.state.lock().awaiting = None;
                let name = inner.name_of(id);
                inner.emit(Event::AutoRefreshed { id, name });
                return;
            }
            if auto { inner.capture_automatically(capture).await } else { inner.ask_about(capture) }
        });
    }

    /// Accepts a download that was started before the user was asked: it
    /// joins the list, in the folder and under the name they chose. What
    /// has been fetched so far is kept.
    pub fn confirm(&self, id: DownloadId, decision: Confirmation) -> Result<()> {
        let mut settle = false;
        let announce;
        {
            let mut state = self.inner.state.lock();
            let queue_running = state.queue_running;
            let item = state.items.get_mut(&id).ok_or_else(|| Error::Protocol("no such download".into()))?;
            item.record.provisional = false;
            if let Some(connections) = decision.connections {
                item.record.connections = Some(connections.clamp(1, crate::settings::MAX_CONNECTIONS));
            }

            let chosen_name = decision.file_name.as_deref().map(filename::sanitize).filter(|n| !n.trim().is_empty());
            if decision.dir.is_some() {
                item.record.auto_dir = false;
            }
            let wanted = Destination {
                dir: decision.dir.unwrap_or_else(|| item.record.dir.clone()),
                file_name: chosen_name.clone().unwrap_or_else(|| item.record.file_name.clone()),
            };
            if wanted.dir != item.record.dir || wanted.file_name != item.record.file_name {
                let has_files = item.record.path().exists() || filename::part_path(&item.record.path()).exists();
                if item.cancel.is_some() {
                    // Being written right now: it moves when the transfer
                    // lets go, without being interrupted.
                    item.record.deliver = Some(wanted);
                } else if has_files {
                    // Moved off this thread: across volumes that is a copy.
                    item.record.deliver = Some(wanted);
                    item.cancel = Some(CancellationToken::new());
                    settle = true;
                } else {
                    item.record.dir = wanted.dir;
                    item.record.file_name = wanted.file_name;
                }
            }
            if chosen_name.is_some() {
                item.record.name_locked = true;
            }

            if decision.later && !item.record.status.is_finished() {
                match (&item.cancel, settle) {
                    (Some(cancel), false) => {
                        item.intent = Intent::Hold;
                        cancel.cancel();
                    }
                    _ if queue_running => {
                        item.record.status = Status::Queued;
                        item.record.queue_run = true;
                    }
                    _ => item.record.status = Status::Scheduled,
                }
            }
            announce = item.record.status.is_finished() && !settle;
        }
        if settle {
            let inner = self.inner.clone();
            self.inner.runtime.spawn(async move { inner.settle(id) });
        } else if announce {
            self.inner.announce(id);
        }
        self.inner.touch();
        self.inner.pump();
        Ok(())
    }

    /// Starts fetching a link the user is still being asked about, when
    /// instant start is on and nothing speaks against it (the address is
    /// already in the list, or it is the fresh link of a stalled download).
    /// The download stays out of the list until [`Manager::confirm`];
    /// [`Manager::discard`] deletes it with everything it fetched.
    pub fn start_ahead(&self, capture: &Capture) -> Option<DownloadId> {
        let id = self.inner.start_provisionally(capture)?;
        self.inner.pump();
        Some(id)
    }

    /// Declines a download that was started before the user was asked.
    /// Everything it fetched is deleted.
    pub fn discard(&self, id: DownloadId) {
        let provisional = self.inner.state.lock().items.get(&id).is_some_and(|i| i.record.provisional);
        if provisional {
            self.remove(id, false);
        }
    }

    /// Sets downloads aside for the queue ("download later"). A running one
    /// stops and keeps what it has.
    pub fn schedule(&self, ids: &[DownloadId]) {
        {
            let mut state = self.inner.state.lock();
            let queue_running = state.queue_running;
            for id in ids {
                let Some(item) = state.items.get_mut(id) else { continue };
                if item.record.status.is_finished() || matches!(item.record.status, Status::NeedsRefresh { .. }) {
                    continue;
                }
                item.retry_at = None;
                item.retries = 0;
                match &item.cancel {
                    Some(cancel) => {
                        item.intent = Intent::Hold;
                        cancel.cancel();
                    }
                    None if queue_running => {
                        item.record.status = Status::Queued;
                        item.record.queue_run = true;
                    }
                    None => item.record.status = Status::Scheduled,
                }
            }
        }
        self.inner.touch();
        self.inner.pump();
    }

    /// Starts everything that was set aside. Returns how many downloads that is.
    pub fn start_queue(&self) -> usize {
        self.inner.start_queue(false)
    }

    /// Stops the queue: what it started goes back to waiting, keeping its data.
    pub fn stop_queue(&self) -> usize {
        self.inner.stop_queue()
    }

    pub fn queue(&self) -> QueueState {
        let state = self.inner.state.lock();
        let listed = || state.items.values().filter(|i| !i.record.provisional);
        QueueState {
            running: state.queue_running,
            waiting: listed().filter(|i| i.record.status == Status::Scheduled).count(),
            remaining: listed().filter(|i| i.record.queue_run && !i.record.status.is_stopped()).count(),
            next_start_in: state.settings.schedule.next_start_in(LocalTime::now()),
        }
    }

    /// Opens the wait for a fresh link: the next link the browser hands over
    /// is tried as the new address of this download before anything else.
    pub fn await_fresh_link(&self, id: DownloadId) {
        self.inner.state.lock().awaiting = Some((id, Instant::now()));
    }

    pub fn stop_awaiting(&self) {
        self.inner.state.lock().awaiting = None;
    }

    /// The download currently waiting for the browser's fresh link.
    pub fn awaiting(&self) -> Option<DownloadId> {
        let state = self.inner.state.lock();
        state.awaiting.filter(|(_, since)| since.elapsed() < AWAIT_FRESH_LINK).map(|(id, _)| id)
    }

    /// The qualities a video page download was last found to offer.
    pub fn variants(&self, id: DownloadId) -> Vec<VariantInfo> {
        self.inner.state.lock().items.get(&id).map(|item| item.variants.clone()).unwrap_or_default()
    }

    /// Chooses another quality for a video page that has not finished. What
    /// was fetched of the old choice is discarded and the download starts
    /// over with the new one.
    pub fn set_variant(&self, id: DownloadId, variant: &str) {
        {
            let mut state = self.inner.state.lock();
            let Some(item) = state.items.get_mut(&id) else { return };
            if item.record.status.is_finished() || item.record.variant.as_deref() == Some(variant) {
                return;
            }
            item.record.variant = Some(variant.to_owned());
            // Another quality can mean another kind of file; the name follows.
            item.record.name_locked = false;
            match &item.cancel {
                Some(cancel) => {
                    item.intent = Intent::Restart;
                    cancel.cancel();
                }
                None => reset_for_restart(item),
            }
        }
        self.inner.touch();
        self.inner.pump();
    }

    /// The helpers video pages need, where they were found: the resolver
    /// (`yt-dlp`) and the joiner (`ffmpeg`).
    pub fn video_tools(&self) -> (Option<PathBuf>, Option<PathBuf>) {
        (self.inner.resolver().ok(), crate::hls::find_ffmpeg())
    }

    #[doc(hidden)]
    pub fn set_resolver_for_tests(&self, resolver: PathBuf) {
        *self.inner.resolver.lock() = Some(resolver);
    }

    /// Computes a checksum of a finished file in the background.
    pub fn checksum(&self, id: DownloadId, algorithm: HashAlgorithm) -> Option<HashJob> {
        let path = {
            let state = self.inner.state.lock();
            let item = state.items.get(&id)?;
            item.record.status.is_finished().then(|| item.record.path())?
        };
        let total = std::fs::metadata(&path).ok()?.len();
        let job = HashJob { algorithm, total, done: Arc::new(AtomicU64::new(0)), cancel: Arc::new(AtomicBool::new(false)), result: Ticket::default() };
        let (done, cancel, filled, inner) = (job.done.clone(), job.cancel.clone(), job.result.clone(), self.inner.clone());
        self.inner.runtime.spawn_blocking(move || {
            let outcome = match checksum::hash_file(&path, algorithm, &done, &cancel) {
                Ok(Some(hex)) => Some(Ok(hex)),
                Ok(None) => None,
                Err(e) => Some(Err(format!("could not read the file: {e}"))),
            };
            if let Some(outcome) = outcome {
                filled.fill(outcome);
                (inner.wake)();
            }
        });
        Some(job)
    }

    /// Lists what is inside a remote ZIP archive without downloading it.
    pub fn peek_archive(&self, request: Request) -> Ticket<Result<ArchiveListing>> {
        let ticket = Ticket::default();
        let inner = self.inner.clone();
        let filled = ticket.clone();
        self.inner.runtime.spawn(async move {
            let listing = async {
                let request = inner.authorized(&request).await;
                let source = HttpSource::new(inner.client()?, request)?;
                let (probe, body) = source.probe().await?;
                drop(body);
                match (probe.resumable, probe.total) {
                    (true, Some(total)) => zip_peek::list(&source, total).await,
                    _ => Err(Error::Unsupported("this server only sends the file in one piece, so it cannot be opened before it is downloaded".into())),
                }
            }
            .await;
            filled.fill(listing);
            (inner.wake)();
        });
        ticket
    }

    /// Reads a page (and, with depth, the pages under it) for links.
    pub fn explore(&self, request: Request, options: GrabOptions) -> GrabJob {
        let job = GrabJob { progress: Arc::new(GrabProgress::default()), cancel: CancellationToken::new(), result: Ticket::default() };
        let (progress, cancel, filled, inner) = (job.progress.clone(), job.cancel.clone(), job.result.clone(), self.inner.clone());
        self.inner.runtime.spawn(async move {
            let found = async {
                let request = inner.authorized(&request).await;
                grab::explore(&inner.client()?, &request, options, &progress, &cancel).await
            }
            .await;
            if !matches!(found, Err(Error::Cancelled)) {
                filled.fill(found);
                (inner.wake)();
            }
        });
        job
    }

    /// Entry point for a page's worth of links sent by the browser.
    pub fn offer_links(&self, batch: LinkBatch) {
        self.inner.emit(Event::Links(batch));
    }

    /// Remembers a login for a site and uses it from now on. The password
    /// goes to the system keychain.
    pub fn save_login(&self, site: &str, username: &str, password: &str) -> std::result::Result<(), String> {
        let pattern = organize::normalize_pattern(site);
        let username = username.trim();
        if pattern.is_empty() || username.is_empty() {
            return Err("a site and a user name are needed".into());
        }
        self.inner.secrets.set(&secrets::site_account(&pattern, username), password)?;
        let mut settings = self.settings();
        match settings.sites.iter_mut().find(|rule| rule.pattern == pattern) {
            Some(rule) => rule.username = username.to_owned(),
            None => settings.sites.push(SiteRule { pattern, connections: None, username: username.to_owned() }),
        }
        self.set_settings(settings).map_err(|e| e.to_string())
    }

    /// Stores or replaces the password of an existing site rule.
    pub fn store_site_password(&self, pattern: &str, username: &str, password: &str) -> std::result::Result<(), String> {
        self.inner.secrets.set(&secrets::site_account(pattern, username), password)
    }

    pub fn forget_site_password(&self, pattern: &str, username: &str) -> std::result::Result<(), String> {
        self.inner.secrets.delete(&secrets::site_account(pattern, username))
    }

    /// The addresses of the given downloads (all of them when `ids` is
    /// empty), one per line, oldest first.
    pub fn export_links(&self, ids: &[DownloadId]) -> String {
        let state = self.inner.state.lock();
        let mut out = String::new();
        for id in state.order.iter().rev() {
            let Some(item) = state.items.get(id) else { continue };
            if item.record.provisional || (!ids.is_empty() && !ids.contains(id)) {
                continue;
            }
            out.push_str(&item.record.request.url);
            out.push('\n');
        }
        out
    }

    /// Asks the interface to come to the front.
    pub fn raise(&self) {
        self.inner.emit(Event::Raise);
    }

    /// Bytes per second across every running download.
    pub fn total_speed(&self) -> f64 {
        let state = self.inner.state.lock();
        state
            .items
            .values()
            .filter(|i| i.record.status == Status::Downloading)
            .map(|i| i.meter.bytes_per_sec())
            .sum()
    }

    /// Stops every task, waits for their final checkpoints and saves.
    pub fn shutdown(&mut self) {
        let Some(runtime) = self.runtime.take() else { return };
        self.inner.closing.store(true, Ordering::SeqCst);
        {
            let mut state = self.inner.state.lock();
            for item in state.items.values_mut() {
                if let Some(cancel) = &item.cancel {
                    item.intent = Intent::Shutdown;
                    cancel.cancel();
                }
            }
        }
        // Tasks checkpoint as they stop; give them a moment to do it.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.inner.state.lock().items.values().all(|i| i.cancel.is_none()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.inner.keep_awake(false);
        let _ = self.inner.save();
        runtime.shutdown_timeout(Duration::from_secs(1));
    }

    #[doc(hidden)]
    pub fn set_tuning_for_tests(&self, tuning: Tuning) {
        *self.inner.tuning.lock() = Some(tuning);
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Builds the HTTP client for a network configuration, starting the
/// authentication bridge first when the proxy needs one.
///
/// Must be called from outside the runtime (it waits on it).
fn connect(runtime: &tokio::runtime::Handle, net: &NetConfig) -> (Result<reqwest::Client>, Option<ProxyBridge>) {
    if !net.proxy.needs_bridge() {
        return (build_client(net, None), None);
    }
    match runtime.block_on(ProxyBridge::start(net.proxy.clone())) {
        Ok(bridge) => (build_client(net, Some(bridge.addr())), Some(bridge)),
        Err(e) => (Err(e), None),
    }
}

fn view_of(item: &Item, now: Instant, settings: &Settings) -> DownloadView {
    let record = &item.record;
    // The user sees where the file is going, not where it is staged.
    let (dir, file_name, staged_at) = match &record.deliver {
        Some(dest) => (dest.dir.clone(), dest.file_name.clone(), Some(record.path())),
        None => (record.dir.clone(), record.file_name.clone(), None),
    };
    let running = record.status == Status::Downloading;
    let downloaded = if record.status.is_active() { item.progress.downloaded().max(record.downloaded) } else { record.downloaded };
    let speed = if running { item.meter.bytes_per_sec() } else { 0.0 };
    let fraction = match (&record.status, record.total) {
        (Status::Completed, _) => Some(1.0),
        (_, Some(0)) => None,
        (_, Some(total)) => Some((downloaded as f64 / total as f64).min(1.0) as f32),
        (_, None) => item.progress.unit_fraction(),
    };
    let eta = match (running, record.total) {
        (true, Some(total)) => eta_secs(total.saturating_sub(downloaded), speed),
        _ => None,
    };
    DownloadView {
        id: record.id,
        category: settings.category_of(&file_name),
        file_name,
        dir,
        staged_at,
        queue_run: record.queue_run,
        provisional: record.provisional,
        variant: record.variant.clone(),
        url: record.request.url.clone(),
        referer: record.request.referer.clone(),
        protocol: record.protocol,
        status: record.status.clone(),
        total: record.total,
        downloaded,
        fraction,
        bytes_per_sec: speed,
        eta_secs: eta,
        connections: if record.status.is_active() { item.progress.connections() } else { 0 },
        resumable: record.resumable,
        content_type: record.content_type.clone(),
        created_at: record.created_at,
        finished_at: record.finished_at,
        note: record.note.clone(),
        retry_in: item.retry_at.map(|at| at.saturating_duration_since(now).as_secs().max(1)),
    }
}

/// The form of an address that downloads are compared by. A video page is
/// the same video whatever playlist or timestamp its link carried.
fn identity(url: &str) -> String {
    let url = url.trim();
    if crate::extractor::is_page(url) { crate::extractor::canonical(url) } else { url.to_owned() }
}

/// Final paths claimed by downloads that have not produced their file yet.
fn reserved_paths(state: &State, except: Option<DownloadId>) -> Vec<PathBuf> {
    state
        .items
        .values()
        .filter(|i| Some(i.record.id) != except && !i.record.status.is_finished())
        .flat_map(|i| std::iter::once(i.record.path()).chain(i.record.deliver.as_ref().map(Destination::path)))
        .collect()
}

/// Moves everything a download has on disk (the finished file, the partial
/// file, its sidecar, a stream's cached pieces) from one final path to
/// another. Either all of it moves or none does.
fn move_set(old: &Path, new: &Path) -> std::io::Result<()> {
    let pairs = [
        (old.to_owned(), new.to_owned()),
        (filename::part_path(old), filename::part_path(new)),
        (filename::meta_path(old), filename::meta_path(new)),
    ];
    let mut moved: Vec<&(PathBuf, PathBuf)> = Vec::new();
    for pair in &pairs {
        if !pair.0.exists() {
            continue;
        }
        if let Err(e) = crate::disk::move_file(&pair.0, &pair.1) {
            // Put back what already moved so the download stays whole.
            for done in moved {
                let _ = crate::disk::move_file(&done.1, &done.0);
            }
            return Err(e);
        }
        moved.push(pair);
    }
    // A stream's cached pieces are worth keeping but not worth failing for:
    // without them the pieces are fetched again.
    let (old_parts, new_parts) = (crate::hls::parts_dir(old), crate::hls::parts_dir(new));
    if old_parts.is_dir() && std::fs::rename(&old_parts, &new_parts).is_err() {
        let _ = std::fs::remove_dir_all(&old_parts);
    }
    Ok(())
}

/// Moves a download to the place the user chose, under a name that is free
/// there. Returns where it ended up.
fn relocate(old: &Path, wanted: &Destination, reserved: &[PathBuf]) -> std::io::Result<Destination> {
    if old == wanted.path() {
        return Ok(wanted.clone());
    }
    std::fs::create_dir_all(&wanted.dir)?;
    let file_name = filename::unique_in(&wanted.dir, &wanted.file_name, &|p| reserved.iter().any(|r| r == p));
    let placed = Destination { dir: wanted.dir.clone(), file_name };
    move_set(old, &placed.path())?;
    Ok(placed)
}

fn refresh_candidate(state: &State, total: Option<u64>, file_name: Option<&str>) -> Option<DownloadId> {
    let total = total?;
    let mut matches = state.order.iter().filter_map(|id| state.items.get(id)).filter(|item| {
        let stalled = matches!(item.record.status, Status::NeedsRefresh { .. });
        stalled && item.record.total == Some(total) && item.record.downloaded > 0
    });
    let first = matches.next()?;
    match matches.next() {
        // One stalled download of exactly this size: that is the one.
        None => Some(first.record.id),
        // Several: only the name can tell them apart.
        Some(second) => {
            let name = file_name?;
            [first, second]
                .into_iter()
                .chain(matches)
                .find(|item| item.record.file_name.eq_ignore_ascii_case(name))
                .map(|item| item.record.id)
        }
    }
}

fn reset_for_restart(item: &mut Item) {
    transfer::discard_partial(&item.record.path());
    crate::hls::discard_parts(&item.record.path());
    item.progress.reset();
    item.meter.reset();
    item.retries = 0;
    item.retry_at = None;
    item.intent = Intent::Run;
    item.record.downloaded = 0;
    item.record.finished_at = None;
    item.record.status = Status::Queued;
}

fn discard_files(record: &Record, trash_file: bool) {
    let path = record.path();
    transfer::discard_partial(&path);
    crate::hls::discard_parts(&path);
    if record.provisional {
        // Fetched ahead of the user's answer, and the answer was no.
        if record.status.is_finished() {
            let _ = std::fs::remove_file(&path);
        }
        return;
    }
    if trash_file && record.status.is_finished() && path.is_file() {
        // The trash, not `unlink`: a slip of the finger stays recoverable.
        if trash::delete(&path).is_err() {
            let _ = std::fs::remove_file(&path);
        }
    }
}


/// Human wording for a failure, shown in the list.
fn failure_message(error: &Error) -> String {
    let text = error.to_string();
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => text,
    }
}

impl Inner {
    fn emit(&self, event: Event) {
        let wants_attention = match &event {
            Event::Raise | Event::Links(_) => true,
            Event::Captured { .. } => self.state.lock().settings.focus_on_capture,
            Event::QueueFinished { when_done, completed, failed } => *when_done != WhenDone::Nothing && completed + failed > 0,
            _ => false,
        };
        self.events.lock().push(event);
        (self.wake)();
        if wants_attention && let Some(hook) = self.attention.lock().as_ref() {
            hook();
        }
    }

    /// Marks the library as changed and asks for a repaint.
    fn touch(&self) {
        self.dirty.store(true, Ordering::Relaxed);
        (self.wake)();
    }

    fn save(&self) -> std::io::Result<()> {
        let library = {
            let state = self.state.lock();
            Library {
                next_id: state.next_id,
                downloads: state.order.iter().filter_map(|id| state.items.get(id)).map(|i| i.record.clone()).collect(),
            }
        };
        self.dirty.store(false, Ordering::Relaxed);
        let bytes = serde_json::to_vec(&library).map_err(std::io::Error::other)?;
        write_atomic(&self.data_dir.join("downloads.json"), &bytes)
    }

    fn insert(&self, new: NewDownload) -> DownloadId {
        let mut state = self.state.lock();
        let id = DownloadId(state.next_id);
        state.next_id += 1;

        let mut request = new.request;
        request.url = identity(&request.url);
        let protocol = request.protocol();
        let reserved = reserved_paths(&state, None);
        let is_reserved = |p: &Path| reserved.iter().any(|r| r == p);

        let wanted = new.file_name.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(filename::sanitize);
        // A guess, until the server says what the file is called.
        let guess = wanted.clone().unwrap_or_else(|| filename::resolve(None, &request.url, new.content_type.as_deref()));
        let auto_dir = new.dir.is_none();
        let dir = new.dir.unwrap_or_else(|| state.settings.folder_for(&guess));
        let (file_name, name_locked) = match wanted {
            Some(name) => (filename::unique_in(&dir, &name, &is_reserved), true),
            None => (guess, false),
        };
        let (status, queue_run) = match (new.later, new.paused, state.queue_running) {
            // Added to a queue that is already running: it takes its turn.
            (true, _, true) => (Status::Queued, true),
            (true, _, false) => (Status::Scheduled, false),
            (false, true, _) => (Status::Paused, false),
            (false, false, _) => (Status::Queued, false),
        };

        let record = Record {
            id,
            request,
            protocol,
            dir,
            file_name,
            name_locked,
            total: new.total,
            downloaded: 0,
            status,
            resumable: None,
            connections: new.connections.map(|c| c.clamp(1, crate::settings::MAX_CONNECTIONS)),
            content_type: new.content_type,
            created_at: unix_now(),
            finished_at: None,
            note: None,
            provisional: new.provisional,
            auto_dir,
            deliver: None,
            queue_run,
            variant: new.variant,
        };
        state.order.insert(0, id);
        state.items.insert(id, Item::new(record));
        drop(state);
        self.dirty.store(true, Ordering::Relaxed);
        id
    }

    /// Starts queued downloads while there are free slots.
    fn pump(self: &Arc<Self>) {
        if self.closing.load(Ordering::SeqCst) {
            return;
        }
        let mut started = Vec::new();
        {
            let mut state = self.state.lock();
            let limit = state.settings.max_active;
            let active = state.items.values().filter(|i| i.cancel.is_some()).count();
            // Oldest first.
            let queued: Vec<DownloadId> = state
                .order
                .iter()
                .rev()
                .copied()
                .filter(|id| {
                    let item = &state.items[id];
                    item.record.status == Status::Queued && item.cancel.is_none()
                })
                .collect();
            // A link the user is being asked about right now does not wait
            // for a slot: that it is already arriving is the point of it.
            let (ahead, waiting): (Vec<DownloadId>, Vec<DownloadId>) = queued.into_iter().partition(|id| state.items[id].record.provisional);
            let free = limit.saturating_sub(active + ahead.len());
            for id in ahead.into_iter().chain(waiting.into_iter().take(free)) {
                let item = state.items.get_mut(&id).expect("listed above");
                let cancel = CancellationToken::new();
                item.cancel = Some(cancel.clone());
                item.intent = Intent::Run;
                item.retry_at = None;
                item.record.status = Status::Connecting;
                item.meter.reset();
                started.push((id, cancel));
            }
        }
        for (id, cancel) in started {
            let inner = self.clone();
            self.runtime.spawn(async move {
                let outcome = inner.attempt(id, &cancel).await;
                inner.finish(id, outcome);
            });
        }
    }

    fn tuning(&self, connections: usize) -> Tuning {
        let mut tuning = self.tuning.lock().clone().unwrap_or_default();
        tuning.connections = connections;
        tuning
    }

    fn client(&self) -> Result<reqwest::Client> {
        self.client.read().clone()
    }

    async fn probe(&self, request: &Request) -> Result<ProbeInfo> {
        let request = &self.authorized(request).await;
        let protocol = request.protocol();
        match protocol {
            Protocol::Http => {
                let source = HttpSource::new(self.client()?, request.clone())?;
                let (probe, body) = source.probe().await?;
                drop(body);
                Ok(ProbeInfo {
                    file_name: probe.file_name,
                    total: probe.total,
                    resumable: probe.resumable,
                    content_type: probe.content_type,
                    final_url: probe.final_url,
                    protocol,
                    note: None,
                    variants: Vec::new(),
                    variant: None,
                })
            }
            Protocol::Ftp => {
                let source = crate::ftp::FtpSource::new(request)?;
                let probe = source.probe().await?;
                Ok(ProbeInfo {
                    file_name: probe.file_name,
                    total: probe.total,
                    resumable: probe.resumable,
                    content_type: None,
                    final_url: probe.final_url,
                    protocol,
                    note: None,
                    variants: Vec::new(),
                    variant: None,
                })
            }
            Protocol::Hls => {
                let plan = crate::hls::plan(&self.client()?, request).await?;
                Ok(ProbeInfo {
                    file_name: plan.file_name(),
                    total: None,
                    resumable: true,
                    content_type: Some("video/mp4".into()),
                    final_url: plan.media_url.clone(),
                    protocol,
                    note: Some(plan.describe()),
                    variants: Vec::new(),
                    variant: None,
                })
            }
            Protocol::Page => {
                let page = crate::extractor::canonical(&request.url);
                let resolved = crate::extractor::resolve(&self.resolver()?, &page).await?;
                let choices = crate::extractor::variants(&resolved);
                let preferred = self.state.lock().settings.video_quality;
                let Some(choice) = crate::extractor::pick(&choices, None, preferred) else {
                    return Err(Error::Unsupported("this page offers nothing that can be saved as a file".into()));
                };
                let mut note = vec![choice.label.clone()];
                note.extend(resolved.duration.map(crate::extractor::clock));
                Ok(ProbeInfo {
                    file_name: filename::sanitize(&format!("{}.{}", resolved.title, choice.container)),
                    total: choice.size,
                    resumable: true,
                    content_type: None,
                    final_url: page,
                    protocol,
                    note: Some(note.join("  \u{00B7}  ")),
                    variants: choices.iter().map(VariantInfo::from).collect(),
                    variant: Some(choice.id.clone()),
                })
            }
            Protocol::Mms => {
                let info = crate::mms::probe(&self.client()?, request).await?;
                Ok(ProbeInfo {
                    file_name: info.file_name,
                    total: info.total,
                    resumable: false,
                    content_type: Some("video/x-ms-asf".into()),
                    final_url: info.url,
                    protocol,
                    note: Some("Windows Media stream".into()),
                    variants: Vec::new(),
                    variant: None,
                })
            }
        }
    }

    /// One attempt at a download, from first contact to an outcome.
    async fn attempt(self: &Arc<Self>, id: DownloadId, cancel: &CancellationToken) -> Outcome {
        let (mut record, progress, connections) = {
            let state = self.state.lock();
            match state.items.get(&id) {
                Some(item) => {
                    // The download's own choice, then the site's rule, then the setting.
                    let site = organize::host_of(&item.record.request.url)
                        .and_then(|host| state.settings.sites.iter().find(|rule| rule.connections.is_some() && rule.matches(&host)).and_then(|rule| rule.connections));
                    let connections = item.record.connections.or(site).unwrap_or(state.settings.connections);
                    (item.record.clone(), item.progress.clone(), connections)
                }
                None => return Outcome::Stopped,
            }
        };
        // The saved login travels with the request, never with the record.
        record.request = self.authorized(&record.request).await;
        let tuning = self.tuning(connections);

        let result = match record.protocol {
            Protocol::Http => self.attempt_ranged(id, &record, &progress, tuning, cancel).await,
            Protocol::Ftp => self.attempt_ranged(id, &record, &progress, tuning, cancel).await,
            Protocol::Hls => self.attempt_hls(id, &record, &progress, tuning, cancel).await,
            Protocol::Mms => self.attempt_mms(id, &record, &progress, cancel).await,
            // The addresses behind a page live for a few hours. When they die
            // mid-transfer the page is read again, once, and what is on disk
            // carries on with the fresh ones.
            Protocol::Page => match self.attempt_page(id, &record, &progress, tuning.clone(), cancel).await {
                Err(Error::Expired(_)) => self.attempt_page(id, &record, &progress, tuning, cancel).await,
                other => other,
            },
        };
        match result {
            Ok(outcome) => outcome,
            Err(Error::Cancelled) => Outcome::Stopped,
            Err(Error::Expired(code)) => Outcome::Expired { code },
            Err(e) => Outcome::Failed(e),
        }
    }

    /// HTTP and FTP: anything that can be probed and fetched by byte range.
    async fn attempt_ranged(
        self: &Arc<Self>,
        id: DownloadId,
        record: &Record,
        progress: &Arc<Progress>,
        tuning: Tuning,
        cancel: &CancellationToken,
    ) -> Result<Outcome> {
        let source = Arc::new(match record.protocol {
            Protocol::Ftp => Source::Ftp(crate::ftp::FtpSource::new(&record.request)?),
            _ => Source::Http(HttpSource::new(self.client()?, record.request.clone())?),
        });

        // The first contact gets a few tries of its own: a download should
        // not fail outright because one handshake was dropped.
        let mut attempt = 0u32;
        let (probe, body) = loop {
            let probed = tokio::select! {
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                probed = source.probe() => probed,
            };
            match probed {
                Ok(found) => break found,
                Err(e) if e.is_transient() && attempt < 3 => {
                    attempt += 1;
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(Error::Cancelled),
                        _ = tokio::time::sleep(Duration::from_millis(600 * (1 << attempt))) => {}
                    }
                }
                Err(e) => return Err(e),
            }
        };

        let final_path = self.adopt_probe(id, &probe, None)?;
        if let Some(item) = self.state.lock().items.get_mut(&id) {
            item.last_modified = probe.validators.last_modified.clone();
        }
        self.set_status(id, Status::Downloading);
        let plan = Plan { source, final_path, probe, body, tuning };
        Ok(transfer::run(plan, progress.clone(), self.limiter.clone(), cancel.clone()).await)
    }

    async fn attempt_hls(
        self: &Arc<Self>,
        id: DownloadId,
        record: &Record,
        progress: &Arc<Progress>,
        tuning: Tuning,
        cancel: &CancellationToken,
    ) -> Result<Outcome> {
        let client = self.client()?;
        let plan = tokio::select! {
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            plan = crate::hls::plan(&client, &record.request) => plan?,
        };
        let probe = Probe {
            total: None,
            resumable: true,
            validators: Default::default(),
            content_type: Some("video/mp4".into()),
            file_name: plan.file_name(),
            final_url: plan.media_url.clone(),
        };
        let final_path = self.adopt_probe(id, &probe, Some(plan.describe()))?;
        self.set_status(id, Status::Downloading);

        let inner = self.clone();
        let on_assembling = move || inner.set_status(id, Status::Assembling);
        crate::hls::run(&client, plan, &final_path, tuning.connections, progress, &self.limiter, cancel, &on_assembling).await
    }

    async fn attempt_mms(
        self: &Arc<Self>,
        id: DownloadId,
        record: &Record,
        progress: &Arc<Progress>,
        cancel: &CancellationToken,
    ) -> Result<Outcome> {
        let client = self.client()?;
        let session = tokio::select! {
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            session = crate::mms::open(&client, &record.request) => session?,
        };
        let probe = Probe {
            total: session.total(),
            resumable: false,
            validators: Default::default(),
            content_type: Some("video/x-ms-asf".into()),
            file_name: session.file_name(),
            final_url: session.url().to_owned(),
        };
        let final_path = self.adopt_probe(id, &probe, Some("Windows Media stream".into()))?;
        self.set_status(id, Status::Downloading);
        crate::mms::run(session, &final_path, progress, &self.limiter, cancel).await
    }

    fn resolver(&self) -> Result<PathBuf> {
        self.resolver.lock().clone().or_else(crate::extractor::find_resolver).ok_or_else(|| {
            Error::Unsupported("video pages need the yt-dlp tool, which is not installed. In Terminal: brew install yt-dlp".into())
        })
    }

    /// A video page: the helper says what is behind it, the streams of the
    /// chosen quality are fetched as files, and picture and sound are joined.
    async fn attempt_page(
        self: &Arc<Self>,
        id: DownloadId,
        record: &Record,
        progress: &Arc<Progress>,
        tuning: Tuning,
        cancel: &CancellationToken,
    ) -> Result<Outcome> {
        let resolver = self.resolver()?;
        let page = crate::extractor::canonical(&record.request.url);
        let resolved = tokio::select! {
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            resolved = crate::extractor::resolve(&resolver, &page) => resolved?,
        };
        let choices = crate::extractor::variants(&resolved);
        // The choice may have changed while this attempt was being set up.
        let (wanted, preferred) = {
            let state = self.state.lock();
            (state.items.get(&id).and_then(|item| item.record.variant.clone()), state.settings.video_quality)
        };
        let Some(choice) = crate::extractor::pick(&choices, wanted.as_deref(), preferred).cloned() else {
            return Err(Error::Unsupported("this page offers nothing that can be saved as a file".into()));
        };
        if let Some(item) = self.state.lock().items.get_mut(&id) {
            item.variants = choices.iter().map(VariantInfo::from).collect();
            item.record.variant = Some(choice.id.clone());
        }

        let mut note = vec![choice.label.clone()];
        note.extend(resolved.duration.map(crate::extractor::clock));
        let probe = Probe {
            total: choice.size,
            resumable: true,
            validators: Default::default(),
            content_type: None,
            file_name: filename::sanitize(&format!("{}.{}", resolved.title, choice.container)),
            final_url: page,
        };
        let final_path = self.adopt_probe(id, &probe, Some(note.join("  \u{00B7}  ")))?;
        self.set_status(id, Status::Downloading);

        let joining = choice.needs_joining();
        let ffmpeg = match joining {
            true => Some(crate::hls::find_ffmpeg().ok_or_else(|| Error::Unsupported("joining picture and sound needs ffmpeg, which is not installed. In Terminal: brew install ffmpeg".into()))?),
            false => None,
        };
        // The two streams wait for each other in a folder next to the file.
        let parts = crate::hls::parts_dir(&final_path);
        if joining {
            std::fs::create_dir_all(&parts).map_err(|e| Error::Disk(e.to_string()))?;
        }

        let mut fetched = 0u64;
        let mut files: Vec<PathBuf> = Vec::new();
        for (name, format) in choice.stages() {
            let target = if joining { parts.join(format!("{name}.{}", format.ext)) } else { final_path.clone() };
            files.push(target.clone());
            // Whole already, from before a pause.
            if joining
                && let Ok(meta) = std::fs::metadata(&target)
                && Some(meta.len()) == format.size
            {
                fetched += meta.len();
                continue;
            }
            progress.begin_stage(fetched);
            let mut request = Request::new(format.url.clone());
            request.headers = format.headers.clone();
            let source = Arc::new(Source::Http(HttpSource::new(self.client()?, request)?));
            let (probe, body) = tokio::select! {
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                probed = source.probe() => probed?,
            };
            let mut tuning = tuning.clone();
            tuning.max_request = Some(format.request_size.unwrap_or(crate::extractor::DEFAULT_REQUEST_SIZE));
            let plan = Plan { source, final_path: target, probe, body, tuning };
            match transfer::run(plan, progress.clone(), self.limiter.clone(), cancel.clone()).await {
                Outcome::Completed { bytes, .. } => fetched += bytes,
                Outcome::Expired { code } => return Err(Error::Expired(code)),
                other => return Ok(other),
            }
        }

        let (Some(ffmpeg), [video, audio]) = (ffmpeg, files.as_slice()) else {
            // One stream: it is the file.
            let bytes = std::fs::metadata(&final_path).map(|m| m.len()).unwrap_or(fetched);
            return Ok(Outcome::Completed { path: final_path, bytes });
        };
        self.set_status(id, Status::Assembling);
        progress.begin_stage(fetched);
        // Something else may have claimed the name while the download ran.
        let output = match final_path.exists() {
            true => {
                let dir = final_path.parent().unwrap_or(Path::new("."));
                let name = final_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                dir.join(filename::unique_in(dir, &name, &|_| false))
            }
            false => final_path.clone(),
        };
        tokio::select! {
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            joined = crate::extractor::join(&ffmpeg, video, audio, &output) => joined?,
        }
        crate::hls::discard_parts(&final_path);
        let bytes = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(fetched);
        Ok(Outcome::Completed { path: output, bytes })
    }

    /// Records what the server said and settles the file's final name.
    fn adopt_probe(&self, id: DownloadId, probe: &Probe, note: Option<String>) -> Result<PathBuf> {
        let mut state = self.state.lock();
        let reserved = reserved_paths(&state, Some(id));
        let sorted_into = state.settings.folder_for(&probe.file_name);
        let item = state.items.get_mut(&id).ok_or(Error::Cancelled)?;
        let record = &mut item.record;
        if !record.name_locked && record.auto_dir {
            // The folder was chosen from a guess at the name. Nothing has
            // been written yet, so the real name can still decide.
            record.dir = sorted_into;
        }
        record.auto_dir = false;
        if probe.total.is_some() {
            record.total = probe.total;
        }
        record.resumable = Some(probe.resumable);
        if probe.content_type.is_some() {
            record.content_type = probe.content_type.clone();
        }
        if note.is_some() {
            record.note = note;
        }
        if !record.name_locked {
            record.file_name =
                filename::unique_in(&record.dir, &probe.file_name, &|p| reserved.iter().any(|r| r == p));
            // From here on the name is on disk; it never changes by itself.
            record.name_locked = true;
        }
        let path = record.path();
        drop(state);
        self.touch();
        Ok(path)
    }

    fn set_status(&self, id: DownloadId, status: Status) {
        if let Some(item) = self.state.lock().items.get_mut(&id) {
            item.record.status = status;
        }
        self.touch();
    }

    /// Moves a download to the place chosen for it while it was being
    /// written, now that nothing is writing. `finished_at` is the completed
    /// file when the transfer ended by completing.
    fn deliver(&self, id: DownloadId, finished_at: Option<&Path>) -> (Option<Destination>, Option<String>) {
        let pending = {
            let mut state = self.state.lock();
            let reserved = reserved_paths(&state, Some(id));
            state.items.get_mut(&id).and_then(|item| item.record.deliver.take().map(|dest| (item.record.path(), dest, reserved)))
        };
        let Some((staged, mut dest, reserved)) = pending else { return (None, None) };
        // A stream can come out in another container than planned (MPEG-TS
        // when it could not be remuxed). The chosen name follows suit, or
        // the file would be labelled as something it is not.
        if let Some(finished) = finished_at
            && let (Some(real), planned) = (finished.extension(), staged.extension())
            && Some(real) != planned
        {
            let (stem, _) = filename::split_extension(&dest.file_name);
            dest.file_name = format!("{stem}.{}", real.to_string_lossy());
        }
        let from = finished_at.map(Path::to_owned).unwrap_or(staged);
        match transfer::blocking(|| relocate(&from, &dest, &reserved)) {
            Ok(placed) => (Some(placed), None),
            Err(e) => (None, Some(format!("Could not move {} to {}: {e}", dest.file_name, dest.dir.display()))),
        }
    }

    /// Carries out a move that was requested for a download that is not
    /// running. The download is marked busy for the duration.
    fn settle(self: &Arc<Self>, id: DownloadId) {
        let (placed, warning) = self.deliver(id, None);
        let finished = {
            let mut state = self.state.lock();
            let Some(item) = state.items.get_mut(&id) else { return };
            item.cancel = None;
            if let Some(placed) = placed {
                item.record.dir = placed.dir;
                item.record.file_name = placed.file_name;
                item.record.name_locked = true;
            }
            item.record.status.is_finished()
        };
        if let Some(text) = warning {
            self.emit(Event::Notice { text, bad: true });
        }
        if finished {
            self.announce(id);
        }
        self.touch();
        self.pump();
    }

    /// Gives a file that has just been completed its labels and date. This
    /// happens before the download is shown as finished, so nobody can open
    /// the file ahead of it.
    fn label(&self, id: DownloadId, path: &Path) {
        let (mark, keep_date, origin) = {
            let state = self.state.lock();
            let Some(item) = state.items.get(&id) else { return };
            let origin = Origin { url: item.record.request.url.clone(), referer: item.record.request.referer.clone(), last_modified: item.last_modified.clone() };
            (state.settings.mark_downloaded, state.settings.keep_server_date, origin)
        };
        transfer::blocking(|| {
            if mark {
                let _ = finishing::mark_downloaded(path, &origin);
            }
            if keep_date && let Some(last_modified) = &origin.last_modified {
                let _ = finishing::keep_server_date(path, last_modified);
            }
        });
    }

    /// Tells the interface a download is finished and where the user wants
    /// it, and runs their command for it.
    fn announce(&self, id: DownloadId) {
        let (name, path, origin, command) = {
            let state = self.state.lock();
            let Some(item) = state.items.get(&id) else { return };
            let origin = Origin { url: item.record.request.url.clone(), referer: item.record.request.referer.clone(), last_modified: item.last_modified.clone() };
            (item.record.file_name.clone(), item.record.path(), origin, state.settings.on_complete.clone())
        };
        if !command.is_empty()
            && let Err(e) = finishing::run_command(&command, &path, &origin)
        {
            self.events.lock().push(Event::Notice { text: format!("The command for finished downloads could not start: {e}"), bad: true });
        }
        self.emit(Event::Completed { id, name, path });
    }

    /// Applies the outcome of a task to the list.
    fn finish(self: &Arc<Self>, id: DownloadId, mut outcome: Outcome) {
        // The transfer has let go of the files. If the user chose another
        // place for them while it ran, this is the moment they move.
        let finished_at = match &outcome {
            Outcome::Completed { path, .. } => Some(path.clone()),
            _ => None,
        };
        let (placed, warning) = self.deliver(id, finished_at.as_deref());
        if let (Some(placed), Outcome::Completed { path, .. }) = (&placed, &mut outcome) {
            *path = placed.path();
        }
        if let Outcome::Completed { path, .. } = &outcome {
            self.label(id, path);
        }

        let mut removed: Option<(Record, bool)> = None;
        let mut events: Vec<Event> = Vec::new();
        let mut announce = false;
        if let Some(text) = warning {
            events.push(Event::Notice { text, bad: true });
        }
        {
            let mut state = self.state.lock();
            let auto_retry = state.settings.auto_retry;
            let Some(item) = state.items.get_mut(&id) else { return };
            item.cancel = None;
            if let Some(placed) = placed {
                item.record.dir = placed.dir;
                item.record.file_name = placed.file_name;
                item.record.name_locked = true;
            }
            item.record.downloaded = item.progress.downloaded().max(if matches!(outcome, Outcome::Completed { .. }) {
                0
            } else {
                // A run that never got to open the file reports zero; keep
                // what the previous run left on disk.
                item.record.downloaded
            });
            let intent = std::mem::replace(&mut item.intent, Intent::Run);
            let name = item.record.file_name.clone();
            let provisional = item.record.provisional;

            match outcome {
                Outcome::Completed { path, bytes } => {
                    if let Some(file_name) = path.file_name() {
                        item.record.file_name = file_name.to_string_lossy().into_owned();
                    }
                    item.record.total = Some(bytes);
                    item.record.downloaded = bytes;
                    item.record.finished_at = Some(unix_now());
                    item.record.status = Status::Completed;
                    item.retries = 0;
                    if let Intent::Remove { trash_file } = intent {
                        // Asked to go just as it crossed the line.
                        let item = state.items.remove(&id).expect("present above");
                        state.order.retain(|other| *other != id);
                        removed = Some((item.record, trash_file));
                    } else {
                        // One still awaiting the user's yes is announced
                        // when they give it.
                        announce = !provisional;
                    }
                }
                Outcome::Stopped => match intent {
                    Intent::Remove { trash_file } => {
                        let item = state.items.remove(&id).expect("present above");
                        state.order.retain(|other| *other != id);
                        removed = Some((item.record, trash_file));
                    }
                    Intent::Restart => reset_for_restart(item),
                    // Interrupted by quitting: pick it up again next launch.
                    Intent::Shutdown => item.record.status = Status::Queued,
                    Intent::Hold => {
                        item.record.status = Status::Scheduled;
                        item.record.queue_run = false;
                    }
                    Intent::Pause | Intent::Run => {
                        item.record.status = Status::Paused;
                        item.record.queue_run = false;
                    }
                },
                Outcome::Expired { code } => {
                    item.record.status = Status::NeedsRefresh { code };
                    if !provisional {
                        events.push(Event::Expired { id, name });
                    }
                }
                Outcome::Failed(error) => {
                    let transient = error.class() == ErrorClass::Transient;
                    item.record.status = Status::Failed { message: failure_message(&error), retryable: error.retryable() };
                    if transient && auto_retry && item.retries < MAX_AUTO_RETRIES && intent == Intent::Run {
                        item.retries += 1;
                        let wait = Duration::from_secs((5u64 << item.retries.min(6)).min(300));
                        item.retry_at = Some(Instant::now() + wait);
                    } else if !provisional {
                        events.push(Event::Failed { id, name, message: failure_message(&error) });
                    }
                }
            }
        }
        if let Some((record, trash_file)) = removed {
            discard_files(&record, trash_file);
        }
        self.events.lock().extend(events);
        if announce {
            self.announce(id);
        }
        self.touch();
        self.pump();
    }

    async fn refresh_link(self: &Arc<Self>, id: DownloadId, mut request: Request) -> Result<()> {
        request.url = request.url.trim().to_owned();
        let (old, known_total, busy) = {
            let state = self.state.lock();
            let item = state.items.get(&id).ok_or_else(|| Error::Protocol("no such download".into()))?;
            (item.record.request.clone(), item.record.total, item.cancel.is_some())
        };
        if busy {
            return Err(Error::Unsupported("Pause the download before changing its link.".into()));
        }
        if request.protocol() != old.protocol() {
            return Err(Error::Mismatch("it uses a different protocol".into()));
        }

        let info = self.probe(&request).await.map_err(|e| match e {
            Error::Expired(code) => Error::Mismatch(format!("the new link does not work either (HTTP {code})")),
            other => other,
        })?;
        // The size is the file's identity. A link to something of another
        // size would be stitched onto bytes it has nothing to do with.
        if let Some(expected) = known_total {
            match info.total {
                Some(found) if found == expected => {}
                Some(found) => {
                    return Err(Error::Mismatch(format!("it is {found} bytes, this download is {expected}")));
                }
                None => return Err(Error::Mismatch("its server does not report a size".into())),
            }
        }

        // The page the file came from is still where a newer link would be
        // found, so keep it when the new request does not bring its own.
        if request.referer.is_none() {
            request.referer = old.referer;
        }
        {
            let mut state = self.state.lock();
            let item = state.items.get_mut(&id).ok_or_else(|| Error::Protocol("no such download".into()))?;
            if item.cancel.is_some() || item.record.status.is_finished() {
                return Err(Error::Unsupported("This download no longer needs a new link.".into()));
            }
            item.record.request = request;
            item.record.status = Status::Queued;
            item.retries = 0;
            item.retry_at = None;
            item.intent = Intent::Run;
        }
        self.touch();
        self.pump();
        Ok(())
    }

    fn name_of(&self, id: DownloadId) -> String {
        self.state.lock().items.get(&id).map(|i| i.record.file_name.clone()).unwrap_or_default()
    }

    /// Adds the saved login for the request's site, if there is one.
    async fn authorized(&self, request: &Request) -> Request {
        let Some(host) = organize::host_of(&request.url) else { return request.clone() };
        let rule = self.state.lock().settings.sites.iter().find(|rule| !rule.username.is_empty() && rule.matches(&host)).cloned();
        let Some(rule) = rule else { return request.clone() };

        // The keychain may ask the user for permission; never on a worker
        // that other downloads share.
        let account = secrets::site_account(&rule.pattern, &rule.username);
        let store = self.secrets.clone();
        let password = tokio::task::spawn_blocking(move || store.get(&account)).await.ok().and_then(|found| found.ok()).flatten().unwrap_or_default();

        let mut out = request.clone();
        match out.protocol() {
            Protocol::Ftp => {
                // An address that names its own account keeps it.
                if let Ok(mut url) = url::Url::parse(&out.url)
                    && url.username().is_empty()
                    && url.set_username(&rule.username).is_ok()
                    && url.set_password(Some(&password)).is_ok()
                {
                    out.url = url.to_string();
                }
            }
            _ => {
                if out.header("authorization").is_none() {
                    let token = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", rule.username, password));
                    out.set_header("Authorization", format!("Basic {token}"));
                }
            }
        }
        out
    }

    /// Puts a captured link in front of the user, fetching it meanwhile when
    /// instant start is on.
    fn ask_about(self: &Arc<Self>, capture: Capture) {
        let provisional = self.start_provisionally(&capture);
        self.emit(Event::Captured { capture, provisional });
        self.pump();
    }

    /// Begins a captured download before the user has confirmed it. Files
    /// and streams alike: wherever the user then wants it, it is moved there
    /// when the transfer lets go of it, so nothing has to be interrupted.
    fn start_provisionally(&self, capture: &Capture) -> Option<DownloadId> {
        {
            let state = self.state.lock();
            if !state.settings.start_immediately {
                return None;
            }
            // Already in the list: the dialog asks what to do about that.
            let url = identity(&capture.request.url);
            if state.items.values().any(|i| !i.record.provisional && i.record.request.url == url) {
                return None;
            }
            // A fresh link for a stalled download is not a new download.
            if capture.size.is_some() && refresh_candidate(&state, capture.size, capture.suggested_name.as_deref()).is_some() {
                return None;
            }
        }
        Some(self.insert(NewDownload {
            request: capture.request.clone(),
            // A stream has no name of its own; the browser's suggestion (the
            // page title) is the best there is. A file is named by its server.
            // A video page is named by its title once the page has been read.
            file_name: capture.suggested_name.clone().filter(|_| capture.media && capture.request.protocol() != Protocol::Page),
            total: capture.size,
            content_type: capture.mime.clone(),
            provisional: true,
            ..Default::default()
        }))
    }

    async fn capture_automatically(self: &Arc<Self>, capture: Capture) {
        // The same address is already in the list and unfinished: one copy
        // is enough. If it had stopped, the new request (fresh cookies)
        // gets it going again.
        let url = identity(&capture.request.url);
        let existing = {
            let state = self.state.lock();
            state.order.iter().copied().find(|id| {
                let record = &state.items[id].record;
                !record.provisional && record.request.url == url && !record.status.is_finished()
            })
        };
        if let Some(id) = existing {
            let revived = self.refresh_link(id, capture.request.clone()).await.is_ok();
            let name = self.name_of(id);
            self.emit(if revived { Event::AutoRefreshed { id, name } } else { Event::AlreadyDownloading { id, name } });
            return;
        }

        // Knowing the size lets a fresh link be recognised as the missing
        // half of a stalled download instead of becoming a duplicate.
        let (total, name) = match capture.size {
            Some(size) => (Some(size), capture.suggested_name.clone()),
            // Reading a video page takes seconds and is done by the download itself.
            None if capture.request.protocol() == Protocol::Page => (None, None),
            None => match self.probe(&capture.request).await {
                Ok(info) => (info.total, capture.suggested_name.clone().or(Some(info.file_name))),
                Err(_) => (None, capture.suggested_name.clone()),
            },
        };
        let candidate = refresh_candidate(&self.state.lock(), total, name.as_deref());
        if let Some(id) = candidate
            && self.refresh_link(id, capture.request.clone()).await.is_ok()
        {
            let name = self.name_of(id);
            self.emit(Event::AutoRefreshed { id, name });
            return;
        }

        let id = self.insert(NewDownload {
            request: capture.request,
            file_name: capture.suggested_name.filter(|_| capture.media),
            total: capture.size,
            content_type: capture.mime,
            ..Default::default()
        });
        let name = self.name_of(id);
        self.emit(Event::AutoStarted { id, name });
        self.pump();
    }

    fn start_queue(self: &Arc<Self>, by_schedule: bool) -> usize {
        let count = {
            let mut state = self.state.lock();
            // Oldest first, so the queue runs in the order things were added.
            let ids: Vec<DownloadId> = state
                .order
                .iter()
                .rev()
                .copied()
                .filter(|id| state.items[id].record.status == Status::Scheduled && !state.items[id].record.provisional)
                .collect();
            for id in &ids {
                let item = state.items.get_mut(id).expect("listed above");
                item.record.status = Status::Queued;
                item.record.queue_run = true;
                item.retries = 0;
                item.retry_at = None;
                item.intent = Intent::Run;
            }
            if !ids.is_empty() {
                state.queue_running = true;
            }
            ids.len()
        };
        if count > 0 {
            self.emit(Event::QueueStarted { count, by_schedule });
            self.touch();
            self.pump();
        }
        count
    }

    fn stop_queue(&self) -> usize {
        let mut stopped = 0;
        {
            let mut state = self.state.lock();
            state.queue_running = false;
            for item in state.items.values_mut().filter(|i| i.record.queue_run) {
                item.record.queue_run = false;
                match (&item.cancel, &item.record.status) {
                    (Some(cancel), _) => {
                        item.intent = Intent::Hold;
                        cancel.cancel();
                        stopped += 1;
                    }
                    (None, Status::Queued) => {
                        item.record.status = Status::Scheduled;
                        stopped += 1;
                    }
                    // Waiting for its automatic retry: back to the queue.
                    (None, Status::Failed { .. }) if item.retry_at.is_some() => {
                        item.retry_at = None;
                        item.record.status = Status::Scheduled;
                        stopped += 1;
                    }
                    _ => {}
                }
            }
        }
        self.touch();
        stopped
    }

    /// Notices when the running queue has nothing left to do.
    fn check_queue(&self) {
        let event = {
            let mut state = self.state.lock();
            if !state.queue_running {
                return;
            }
            let (mut pending, mut completed, mut failed) = (0usize, 0usize, 0usize);
            for item in state.items.values().filter(|i| i.record.queue_run) {
                match &item.record.status {
                    Status::Completed => completed += 1,
                    Status::Queued | Status::Connecting | Status::Downloading | Status::Assembling => pending += 1,
                    Status::Failed { .. } if item.retry_at.is_some() => pending += 1,
                    Status::Failed { .. } | Status::NeedsRefresh { .. } => failed += 1,
                    Status::Paused | Status::Scheduled => {}
                }
            }
            if pending > 0 {
                return;
            }
            state.queue_running = false;
            for item in state.items.values_mut() {
                item.record.queue_run = false;
            }
            Event::QueueFinished { when_done: state.settings.schedule.when_done, completed, failed }
        };
        self.emit(event);
    }

    /// Holds off idle sleep while downloads are running, if the user wants that.
    fn keep_awake(&self, busy: bool) {
        #[cfg(target_os = "macos")]
        {
            let wanted = busy && self.state.lock().settings.keep_awake;
            let mut helper = self.awake.lock();
            // A helper that has exited is forgotten.
            if helper.as_mut().is_some_and(|child| !matches!(child.try_wait(), Ok(None))) {
                *helper = None;
            }
            match (wanted, helper.is_some()) {
                (true, false) => {
                    // `-i` prevents idle sleep. `-w` ties the helper's life
                    // to this process, so it cannot outlive a crash.
                    *helper = std::process::Command::new("/usr/bin/caffeinate")
                        .args(["-i", "-w", &std::process::id().to_string()])
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                        .ok();
                }
                (false, true) => {
                    if let Some(mut child) = helper.take() {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                }
                _ => {}
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (busy, &self.awake);
        }
    }
}

/// Samples speeds, fires automatic retries and saves the library.
async fn ticker(inner: Arc<Inner>) {
    let mut last_save = Instant::now();
    let mut last_clock = Instant::now();
    let mut local = LocalTime::now();
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        if inner.closing.load(Ordering::SeqCst) {
            break;
        }
        let now = Instant::now();
        let mut any_active = false;
        let mut transferring = false;
        let mut requeued = false;
        {
            let mut state = inner.state.lock();
            for item in state.items.values_mut() {
                if item.record.status.is_active() {
                    any_active = true;
                    transferring = true;
                    item.meter.sample(now, item.progress.net_bytes());
                    let downloaded = item.progress.downloaded();
                    if downloaded > item.record.downloaded {
                        item.record.downloaded = downloaded;
                        inner.dirty.store(true, Ordering::Relaxed);
                    }
                } else if item.retry_at.is_some_and(|at| at <= now) {
                    item.retry_at = None;
                    item.record.status = Status::Queued;
                    requeued = true;
                } else if item.retry_at.is_some() {
                    // The countdown in the list ticks.
                    any_active = true;
                }
            }
        }
        if requeued {
            inner.pump();
        }
        // Once a second: the wall clock (for the schedule), the queue, and
        // the hold on idle sleep.
        if now.duration_since(last_clock) >= Duration::from_secs(1) {
            last_clock = now;
            let previous = std::mem::replace(&mut local, LocalTime::now());
            let crossing = inner.state.lock().settings.schedule.crossing(previous, local);
            match crossing {
                Some(Crossing::Start) => {
                    inner.start_queue(true);
                }
                Some(Crossing::Stop) => {
                    inner.stop_queue();
                }
                None => {}
            }
            inner.check_queue();
            inner.keep_awake(transferring);
        }
        if any_active || requeued {
            (inner.wake)();
        }
        if inner.dirty.load(Ordering::Relaxed) && now.duration_since(last_save) >= SAVE_EVERY {
            last_save = now;
            let inner = inner.clone();
            let _ = tokio::task::spawn_blocking(move || inner.save()).await;
        }
    }
}
