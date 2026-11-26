//! One run of one download: from "the server described the file" to
//! "the last byte is on disk", or to whatever stopped it on the way.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::disk::{PartFile, ResumeMeta};
use crate::error::{Error, ErrorClass, Result};
use crate::filename;
use crate::http::same_resource;
use crate::rate::Limiter;
use crate::segments::{Segment, SegmentMap, StealPolicy};
use crate::source::{Body, Probe, Source};
use crate::types::SegmentSnapshot;

/// Knobs of a run. The defaults are what the application ships with; tests
/// shrink them to exercise the same paths on small files.
#[derive(Clone, Debug)]
pub struct Tuning {
    /// Upper bound on simultaneous connections for this download.
    pub connections: usize,
    /// When an idle worker may take half of another worker's range.
    pub steal: StealPolicy,
    /// How often the byte map is flushed to the sidecar.
    pub checkpoint_every: Duration,
    /// Consecutive failed attempts, with no byte gained, before a worker
    /// hands its range back.
    pub retry_limit: u32,
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
    /// Network chunks are coalesced up to this size before hitting the disk.
    pub flush_bytes: usize,
    /// ...or after this long, whichever comes first, so progress keeps moving
    /// on a slow link.
    pub flush_every: Duration,
    /// The most bytes to ask for in one request. Some hosts serve modest
    /// ranges at full speed and slow down long ones; a connection then walks
    /// its range in pieces of this size.
    pub max_request: Option<u64>,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            connections: 8,
            steal: StealPolicy::default(),
            checkpoint_every: Duration::from_millis(1500),
            retry_limit: 6,
            backoff_base: Duration::from_millis(400),
            backoff_cap: Duration::from_secs(10),
            flush_bytes: 256 * 1024,
            flush_every: Duration::from_millis(200),
            max_request: None,
        }
    }
}

/// Live numbers of a run, readable from any thread without blocking it.
#[derive(Debug, Default)]
pub struct Progress {
    map: Mutex<Option<Arc<SegmentMap>>>,
    /// Bytes on disk when the download is one sequential stream.
    stream_bytes: AtomicU64,
    /// Every payload byte received from the network, for the speed meter.
    net_bytes: AtomicU64,
    connections: AtomicU32,
    units_done: AtomicU32,
    units_total: AtomicU32,
    /// Bytes of earlier stages, for a download made of several files (a
    /// picture stream, then its sound).
    base: AtomicU64,
}

impl Progress {
    /// Bytes safely written.
    pub fn downloaded(&self) -> u64 {
        self.base.load(Ordering::Relaxed)
            + match self.map.lock().as_ref() {
                Some(map) => map.downloaded(),
                None => self.stream_bytes.load(Ordering::Relaxed),
            }
    }

    /// Starts a new stage: what the stages before it fetched is carried as
    /// a fixed amount, and the byte map starts afresh.
    pub(crate) fn begin_stage(&self, already: u64) {
        *self.map.lock() = None;
        self.stream_bytes.store(0, Ordering::Relaxed);
        self.base.store(already, Ordering::Relaxed);
    }

    pub fn net_bytes(&self) -> u64 {
        self.net_bytes.load(Ordering::Relaxed)
    }

    pub fn connections(&self) -> u32 {
        self.connections.load(Ordering::Relaxed)
    }

    /// The byte map, or an empty list for a sequential stream.
    pub fn segments(&self) -> Vec<SegmentSnapshot> {
        self.map.lock().as_ref().map(|m| m.snapshot()).unwrap_or_default()
    }

    pub(crate) fn add_net(&self, bytes: u64) {
        self.net_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn set_stream_bytes(&self, bytes: u64) {
        self.stream_bytes.store(bytes, Ordering::Relaxed);
    }

    pub(crate) fn connection_opened(&self) {
        self.connections.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn connection_closed(&self) {
        self.connections.fetch_sub(1, Ordering::Relaxed);
    }

    /// Progress in whole units (stream segments), for downloads whose size
    /// in bytes is not known in advance.
    pub(crate) fn set_units(&self, done: u32, total: u32) {
        self.units_done.store(done, Ordering::Relaxed);
        self.units_total.store(total, Ordering::Relaxed);
    }

    pub(crate) fn unit_done(&self) {
        self.units_done.fetch_add(1, Ordering::Relaxed);
    }

    /// Share of units finished, when the download is counted in units.
    pub fn unit_fraction(&self) -> Option<f32> {
        let total = self.units_total.load(Ordering::Relaxed);
        (total > 0).then(|| (self.units_done.load(Ordering::Relaxed) as f32 / total as f32).min(1.0))
    }

    pub(crate) fn add_stream_bytes(&self, bytes: u64) {
        self.stream_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn attach(&self, map: Arc<SegmentMap>) {
        *self.map.lock() = Some(map);
    }

    /// Clears per-run state so a restarted download does not show stale data.
    pub fn reset(&self) {
        *self.map.lock() = None;
        self.stream_bytes.store(0, Ordering::Relaxed);
        self.connections.store(0, Ordering::Relaxed);
        self.units_done.store(0, Ordering::Relaxed);
        self.units_total.store(0, Ordering::Relaxed);
        self.base.store(0, Ordering::Relaxed);
    }
}

/// Everything a run needs.
#[derive(Debug)]
pub struct Plan {
    pub source: Arc<Source>,
    /// Where the finished file goes. The in-progress file and the sidecar sit
    /// next to it with added extensions.
    pub final_path: PathBuf,
    pub probe: Probe,
    /// The response the probe left open, when the server ignored the range
    /// and started sending the whole file.
    pub body: Option<Body>,
    pub tuning: Tuning,
}

/// How a run ended.
#[derive(Debug)]
pub enum Outcome {
    Completed { path: PathBuf, bytes: u64 },
    /// The cancellation token fired. Progress is checkpointed.
    Stopped,
    /// The link died mid-transfer. Progress is checkpointed and waits for a
    /// fresh URL.
    Expired { code: u16 },
    Failed(Error),
}

pub async fn run(plan: Plan, progress: Arc<Progress>, limiter: Arc<Limiter>, cancel: CancellationToken) -> Outcome {
    let Plan { source, final_path, probe, body, tuning } = plan;
    let result = match probe.total {
        Some(total) if probe.resumable => {
            // The probe response is not needed when ranges work.
            drop(body);
            segmented(&source, &final_path, &probe, total, &tuning, &progress, &limiter, &cancel).await
        }
        total => stream(body, &final_path, total, &tuning, &progress, &limiter, &cancel).await,
    };
    match result {
        Ok(outcome) => outcome,
        Err(Error::Cancelled) => Outcome::Stopped,
        Err(Error::Expired(code)) => Outcome::Expired { code },
        Err(e) => Outcome::Failed(e),
    }
}

/// State shared by the workers of one segmented run.
struct Shared {
    source: Arc<Source>,
    part: PartFile,
    map: Arc<SegmentMap>,
    total: u64,
    tuning: Tuning,
    progress: Arc<Progress>,
    limiter: Arc<Limiter>,
    /// Fires on user cancellation and when any worker hits an error that
    /// dooms the whole run.
    abort: CancellationToken,
    /// The error that fired `abort`, if it was an error.
    failure: Mutex<Option<Error>>,
    meta_path: PathBuf,
}

impl Shared {
    fn fail(&self, error: Error) {
        let mut slot = self.failure.lock();
        if slot.is_none() {
            *slot = Some(error);
        }
        self.abort.cancel();
    }

    /// Makes everything written so far durable, then records it.
    ///
    /// Order matters: the byte map is captured first and the data file is
    /// flushed after, so the sidecar can only ever claim bytes that were
    /// already handed to the OS before the flush began. A power cut between
    /// the two steps leaves the previous, still truthful, checkpoint.
    fn checkpoint(&self) -> std::io::Result<()> {
        let segments = self.map.snapshot();
        self.part.sync()?;
        let validators = self.source.validators();
        ResumeMeta::new(
            &self.source.resource_url(),
            self.total,
            validators.etag,
            validators.last_modified,
            segments,
        )
        .save(&self.meta_path)
    }
}

#[allow(clippy::too_many_arguments)]
async fn segmented(
    source: &Arc<Source>,
    final_path: &Path,
    probe: &Probe,
    total: u64,
    tuning: &Tuning,
    progress: &Arc<Progress>,
    limiter: &Arc<Limiter>,
    cancel: &CancellationToken,
) -> Result<Outcome> {
    let part_path = filename::part_path(final_path);
    let meta_path = filename::meta_path(final_path);

    if total == 0 {
        PartFile::create(&part_path, Some(0))?;
        let path = finalize(&part_path, final_path, &meta_path)?;
        return Ok(Outcome::Completed { path, bytes: 0 });
    }

    let (part, map) = match restore(&part_path, &meta_path, probe, total, tuning.steal)? {
        Some(restored) => restored,
        None => {
            let part = blocking(|| PartFile::create(&part_path, Some(total)))?;
            (part, SegmentMap::new(total, tuning.steal))
        }
    };
    let map = Arc::new(map);
    progress.attach(map.clone());

    let shared = Arc::new(Shared {
        source: source.clone(),
        part,
        map: map.clone(),
        total,
        tuning: tuning.clone(),
        progress: progress.clone(),
        limiter: limiter.clone(),
        abort: cancel.child_token(),
        failure: Mutex::new(None),
        meta_path: meta_path.clone(),
    });

    // The sidecar exists from the first moment, so even a crash before the
    // first periodic checkpoint resumes instead of starting over.
    {
        let shared = shared.clone();
        tokio::task::spawn_blocking(move || shared.checkpoint())
            .await
            .map_err(|e| Error::Disk(e.to_string()))??;
    }

    // The periodic checkpointer is tied to this future: if the run is dropped
    // for any reason, it stops with it instead of writing on as an orphan.
    let mut ticker = {
        let shared = shared.clone();
        let stop = shared.abort.child_token();
        let handle = tokio::spawn({
            let stop = stop.clone();
            async move {
                loop {
                    tokio::select! {
                        _ = stop.cancelled() => break,
                        _ = tokio::time::sleep(shared.tuning.checkpoint_every) => {}
                    }
                    let shared = shared.clone();
                    match tokio::task::spawn_blocking(move || (shared.checkpoint(), shared)).await {
                        Ok((Ok(()), _)) => {}
                        Ok((Err(e), shared)) => {
                            shared.fail(e.into());
                            break;
                        }
                        Err(_) => break,
                    }
                }
            }
        });
        (stop, AbortOnDrop(handle))
    };

    let mut concurrency = tuning.connections.max(1);
    let mut stalled_rounds = 0u32;
    let mut last_error: Option<Error> = None;

    // A round ends when every worker has run out of work or given up. Ranges
    // abandoned by workers that gave up are picked up by the next round, as
    // long as rounds keep making progress.
    let verdict: Result<()> = loop {
        let before = map.downloaded();
        let mut workers = JoinSet::new();
        // Every opening range is handed out before any worker starts. A worker
        // spawned earlier would read its end, send the request, and only then
        // have the next worker cut its range in half under it.
        let opening: Vec<Arc<Segment>> = std::iter::from_fn(|| map.acquire()).take(concurrency).collect();
        if opening.is_empty() {
            break Ok(());
        }
        for segment in opening {
            workers.spawn(worker(shared.clone(), segment));
        }

        let mut gave_up = 0usize;
        while let Some(joined) = workers.join_next().await {
            match joined {
                Ok(None) => {}
                Ok(Some(error)) => {
                    gave_up += 1;
                    last_error = Some(error);
                }
                Err(e) => shared.fail(Error::Protocol(format!("a download worker crashed: {e}"))),
            }
        }

        if let Some(error) = shared.failure.lock().take() {
            break Err(error);
        }
        if cancel.is_cancelled() {
            break Err(Error::Cancelled);
        }
        if map.is_complete() {
            break Ok(());
        }

        stalled_rounds = if map.downloaded() > before { 0 } else { stalled_rounds + 1 };
        if stalled_rounds >= 2 {
            break Err(last_error.take().unwrap_or(Error::Timeout));
        }
        // Workers that could not get a connection going suggest the server
        // will not accept this many at once.
        concurrency = concurrency.saturating_sub(gave_up).max(1);
    };

    ticker.0.cancel();
    let _ = (&mut ticker.1.0).await;
    drop(ticker);

    // Workers are gone; this is the last reference and the handle closes
    // with it (Windows will not rename an open file).
    let shared = Arc::try_unwrap(shared).map_err(|_| Error::Protocol("download state is still in use".into()))?;

    match verdict {
        Ok(()) if map.is_complete() => {
            blocking(|| shared.part.sync())?;
            drop(shared);
            let path = finalize(&part_path, final_path, &meta_path)?;
            Ok(Outcome::Completed { path, bytes: total })
        }
        Ok(()) => Err(Error::Protocol("the download stopped before every byte was fetched".into())),
        Err(Error::RangeIgnored) if map.downloaded() == 0 => {
            // The server advertised ranges and then sent the whole file. Take
            // it as one stream instead of failing.
            let body = source.open(0, total, total).await?;
            drop(shared);
            let _ = std::fs::remove_file(&meta_path);
            progress.reset();
            stream(Some(body), final_path, Some(total), tuning, progress, limiter, cancel).await
        }
        Err(error) => {
            // Whatever stopped the run, what was written is kept.
            blocking(|| shared.checkpoint())?;
            Err(error)
        }
    }
}

/// Tries to continue from a previous run's files.
///
/// `Ok(None)` means there is nothing usable and the download starts fresh.
/// An error means the partial data belongs to a different version of the
/// file, which the user has to decide about.
fn restore(
    part_path: &Path,
    meta_path: &Path,
    probe: &Probe,
    total: u64,
    policy: StealPolicy,
) -> Result<Option<(PartFile, SegmentMap)>> {
    let Some(meta) = ResumeMeta::load(meta_path) else { return Ok(None) };
    if !part_path.exists() {
        return Ok(None);
    }
    if meta.total != total {
        return Err(Error::Changed);
    }
    // Validators are only comparable when they describe the same resource. A
    // mirror network may answer from a different server each time, with a
    // different ETag for identical bytes; the size check above still holds.
    let saved = crate::source::Validators { etag: meta.etag.clone(), last_modified: meta.last_modified.clone() };
    if same_resource(&meta.url, &probe.final_url) && saved.conflicts_with(&probe.validators) {
        return Err(Error::Changed);
    }
    let Ok(part) = PartFile::open_existing(part_path, total) else { return Ok(None) };
    match SegmentMap::restore(total, &meta.segments, policy) {
        Ok(map) => Ok(Some((part, map))),
        Err(_) => Ok(None),
    }
}

/// A worker: fetch a range, then go and find another one.
///
/// Returns the error it gave up on, or `None` when it simply ran out of work.
async fn worker(shared: Arc<Shared>, first: Arc<Segment>) -> Option<Error> {
    let mut next = Some(first);
    loop {
        let segment = match next.take().or_else(|| shared.map.acquire()) {
            Some(segment) => segment,
            None => return None,
        };
        let result = fetch_with_retries(&shared, &segment).await;
        shared.map.release(&segment);
        match result {
            Ok(()) => {}
            Err(error) => match error.class() {
                ErrorClass::Cancelled => return None,
                // This worker is out of patience; the range is back in the
                // pool for the others.
                ErrorClass::Transient => return Some(error),
                ErrorClass::Expired | ErrorClass::Fatal => {
                    shared.fail(error);
                    return None;
                }
            },
        }
    }
}

async fn fetch_with_retries(shared: &Shared, segment: &Segment) -> Result<()> {
    let mut failures = 0u32;
    loop {
        let before = segment.pos();
        let error = match fetch_once(shared, segment).await {
            // One piece of a range that is fetched in pieces: on to the next.
            Ok(()) if shared.tuning.max_request.is_some() && segment.remaining() > 0 => {
                failures = 0;
                continue;
            }
            Ok(()) => return Ok(()),
            Err(e) if e.is_transient() => e,
            Err(e) => return Err(e),
        };

        // Any byte gained means the link works; start counting again.
        failures = if segment.pos() > before { 1 } else { failures + 1 };
        if failures > shared.tuning.retry_limit {
            return Err(error);
        }
        let mut wait = backoff(&shared.tuning, failures);
        if let Error::Busy { retry_after: Some(asked), .. } = &error {
            wait = wait.max((*asked).min(Duration::from_secs(30)));
        }
        tokio::select! {
            _ = shared.abort.cancelled() => return Err(Error::Cancelled),
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

/// Exponential backoff with jitter, so workers that failed together do not
/// all knock on the server again at the same instant.
fn backoff(tuning: &Tuning, attempt: u32) -> Duration {
    let exp = tuning.backoff_base.saturating_mul(1u32 << attempt.saturating_sub(1).min(16));
    let capped = exp.min(tuning.backoff_cap);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    // 75%..125%
    capped.mul_f64(0.75 + (nanos % 1000) as f64 / 2000.0)
}

/// One request for one range, written straight to its place in the file.
async fn fetch_once(shared: &Shared, segment: &Segment) -> Result<()> {
    let start = segment.pos();
    let mut requested_end = segment.end();
    if start >= requested_end {
        return Ok(());
    }
    if let Some(most) = shared.tuning.max_request {
        requested_end = requested_end.min(start + most.max(1));
    }

    let mut body = tokio::select! {
        _ = shared.abort.cancelled() => return Err(Error::Cancelled),
        opened = shared.source.open(start, requested_end, shared.total) => opened?,
    };

    shared.progress.connection_opened();
    let result = pump(shared, segment, &mut body, start, requested_end).await;
    shared.progress.connection_closed();
    result
}

async fn pump(shared: &Shared, segment: &Segment, body: &mut Body, start: u64, requested_end: u64) -> Result<()> {
    // `at` is the file offset of the first byte in `buffer`.
    let mut at = start;
    let mut buffer: Vec<u8> = Vec::new();
    let mut last_flush = Instant::now();

    // Writes the buffered bytes that still belong to this segment (a stealer
    // may have lowered the end) and publishes the new position.
    let flush = |at: &mut u64, buffer: &mut Vec<u8>| -> Result<()> {
        let n = segment.writable(*at, buffer.len());
        if n > 0 {
            blocking(|| shared.part.write_at(&buffer[..n], *at))?;
            *at += n as u64;
            segment.commit(*at);
        }
        buffer.clear();
        Ok(())
    };

    loop {
        let chunk = tokio::select! {
            _ = shared.abort.cancelled() => {
                flush(&mut at, &mut buffer)?;
                return Err(Error::Cancelled);
            }
            chunk = body.chunk() => chunk,
        };
        let bytes = match chunk {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                flush(&mut at, &mut buffer)?;
                // The stream ended. That is success only if the segment did
                // too, or if everything that was asked for has arrived.
                return if segment.remaining() == 0 || at >= requested_end { Ok(()) } else { Err(Error::Truncated) };
            }
            Err(e) => {
                // Keep what arrived before the connection broke.
                flush(&mut at, &mut buffer)?;
                return Err(e);
            }
        };

        shared.progress.add_net(bytes.len() as u64);
        shared.limiter.take(bytes.len()).await;

        let buffered_end = at + buffer.len() as u64;
        let take = segment.writable(buffered_end, bytes.len());
        if buffer.is_empty() && take >= shared.tuning.flush_bytes {
            // Big chunk on a fast link: skip the copy.
            blocking(|| shared.part.write_at(&bytes[..take], at))?;
            at += take as u64;
            segment.commit(at);
            last_flush = Instant::now();
        } else {
            buffer.extend_from_slice(&bytes[..take]);
        }

        let reached = at + buffer.len() as u64;
        if take < bytes.len() || reached >= segment.end() || reached >= requested_end {
            // End of the segment (its natural end or a stealer's cut), or of
            // the piece of it that this request asked for.
            flush(&mut at, &mut buffer)?;
            if reached >= requested_end {
                // The response is exhausted too. Reading its end-of-stream
                // marker lets the connection go back to the pool warm.
                let _ = tokio::time::timeout(Duration::from_millis(250), body.chunk()).await;
            }
            return Ok(());
        }

        if buffer.len() >= shared.tuning.flush_bytes || last_flush.elapsed() >= shared.tuning.flush_every {
            flush(&mut at, &mut buffer)?;
            last_flush = Instant::now();
        }
    }
}

/// A server without range support: one connection, front to back.
async fn stream(
    body: Option<Body>,
    final_path: &Path,
    total: Option<u64>,
    tuning: &Tuning,
    progress: &Arc<Progress>,
    limiter: &Arc<Limiter>,
    cancel: &CancellationToken,
) -> Result<Outcome> {
    let mut body = body.ok_or_else(|| Error::Protocol("the server did not send the file".into()))?;
    let part_path = filename::part_path(final_path);
    let meta_path = filename::meta_path(final_path);
    // Whatever a previous attempt left cannot be continued without ranges.
    let _ = std::fs::remove_file(&meta_path);
    let part = blocking(|| PartFile::create(&part_path, total))?;
    progress.set_stream_bytes(0);

    let mut at = 0u64;
    let mut buffer: Vec<u8> = Vec::with_capacity(tuning.flush_bytes);
    let mut last_flush = Instant::now();
    progress.connection_opened();

    let result: Result<()> = loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => break Err(Error::Cancelled),
            chunk = body.chunk() => chunk,
        };
        let bytes = match chunk {
            Ok(Some(bytes)) => bytes,
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        };
        progress.add_net(bytes.len() as u64);
        limiter.take(bytes.len()).await;
        buffer.extend_from_slice(&bytes);

        if buffer.len() >= tuning.flush_bytes || last_flush.elapsed() >= tuning.flush_every {
            if let Err(e) = blocking(|| part.write_at(&buffer, at)) {
                break Err(e.into());
            }
            at += buffer.len() as u64;
            buffer.clear();
            progress.set_stream_bytes(at);
            last_flush = Instant::now();
        }
    };
    progress.connection_closed();
    result?;

    blocking(|| part.write_at(&buffer, at))?;
    at += buffer.len() as u64;
    progress.set_stream_bytes(at);

    match total {
        Some(total) if at < total => return Err(Error::Truncated),
        Some(total) if at > total => return Err(Error::Changed),
        // The size was unknown or a reservation overshot; the file is exactly
        // what was received.
        _ => blocking(|| part.set_len(at))?,
    }
    blocking(|| part.sync())?;
    drop(part);
    let path = finalize(&part_path, final_path, &meta_path)?;
    Ok(Outcome::Completed { path, bytes: at })
}

/// Aborts a background task when its owner goes away.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Gives the finished file its real name and removes the sidecar.
pub(crate) fn finalize(part_path: &Path, final_path: &Path, meta_path: &Path) -> Result<PathBuf> {
    // Something else may have claimed the name while the download ran.
    let target = if final_path.exists() {
        let dir = final_path.parent().unwrap_or(Path::new("."));
        let name = final_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        dir.join(filename::unique_in(dir, &name, &|_| false))
    } else {
        final_path.to_owned()
    };
    std::fs::rename(part_path, &target)?;
    let _ = std::fs::remove_file(meta_path);
    crate::disk::remove_stale_temporaries(meta_path);
    Ok(target)
}

/// Runs a short blocking call (a positional write, a flush) without stalling
/// the async workers that share this thread.
pub(crate) fn blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(f),
        _ => f(),
    }
}

/// Removes the in-progress file and sidecar of a download.
pub fn discard_partial(final_path: &Path) {
    let _ = std::fs::remove_file(filename::part_path(final_path));
    let meta_path = filename::meta_path(final_path);
    let _ = std::fs::remove_file(&meta_path);
    crate::disk::remove_stale_temporaries(&meta_path);
}

/// Bytes a previous run left on disk for this destination, if it can be resumed.
pub fn resumable_bytes(final_path: &Path) -> Option<(u64, u64)> {
    let meta = ResumeMeta::load(&filename::meta_path(final_path))?;
    filename::part_path(final_path).exists().then(|| (meta.downloaded(), meta.total))
}
