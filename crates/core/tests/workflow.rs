//! The workflows around a download: starting before the user has answered,
//! the queue of deferred downloads, per-site rules, sorting, duplicates,
//! finishing touches, checksums, archive previews and the link grabber.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use siphon_core::checksum::HashAlgorithm;
use siphon_core::filename::{meta_path, part_path};
use siphon_core::grab::{GrabOptions, LinkKind};
use siphon_core::net::ProxyMode;
use siphon_core::organize::SiteRule;
use siphon_core::schedule::{LocalTime, WhenDone};
use siphon_core::secrets::Secrets;
use siphon_core::segments::StealPolicy;
use siphon_core::transfer::Tuning;
use siphon_core::types::{DownloadId, Request, Status};
use siphon_core::{Capture, Confirmation, DownloadView, Error, Event, Manager, NewDownload, Protocol, Settings};
use siphon_testserver::{Route, TestServer, pattern, verify_file};

const MIB: u64 = 1024 * 1024;

struct Fixture {
    root: PathBuf,
    manager: Manager,
    server: TestServer,
    server_runtime: tokio::runtime::Runtime,
}

impl Fixture {
    fn new(name: &str) -> Self {
        Self::with(name, |_| {})
    }

    fn with(name: &str, configure: impl FnOnce(&mut Settings)) -> Self {
        let root = std::env::temp_dir().join(format!("siphon-flow-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("downloads")).unwrap();
        let manager = open(&root, configure);
        let server_runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let server = server_runtime.block_on(TestServer::start());
        Self { root, manager, server, server_runtime }
    }

    fn downloads(&self) -> PathBuf {
        self.root.join("downloads")
    }

    fn wait(&self, id: DownloadId, what: &str, test: impl Fn(&DownloadView) -> bool) -> DownloadView {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let view = self.manager.get(id).unwrap_or_else(|| panic!("download {id} vanished while waiting for: {what}"));
            if test(&view) {
                return view;
            }
            assert!(Instant::now() < deadline, "timed out waiting for: {what}; last state {:?}", view.status);
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits for an event and returns it; other events seen meanwhile are kept in `seen`.
    fn event(&self, seen: &mut Vec<Event>, what: &str, test: impl Fn(&Event) -> bool) -> Event {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            seen.extend(self.manager.drain_events());
            if let Some(at) = seen.iter().position(&test) {
                return seen.remove(at);
            }
            assert!(Instant::now() < deadline, "timed out waiting for event: {what}; saw {seen:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn capture(&self, path: &str) -> (Capture, Option<DownloadId>) {
        let capture = Capture { request: Request::new(self.server.url(path)).with_referer("https://site.example/page"), ..Default::default() };
        self.manager.capture(capture.clone());
        let mut seen = Vec::new();
        match self.event(&mut seen, "the capture", |e| matches!(e, Event::Captured { .. })) {
            Event::Captured { provisional, .. } => (capture, provisional),
            _ => unreachable!(),
        }
    }

    fn reopen(mut self, configure: impl FnOnce(&mut Settings)) -> Self {
        self.manager.shutdown();
        let manager = open(&self.root, configure);
        Self { root: self.root.clone(), manager, server: self.server, server_runtime: self.server_runtime }
    }

    fn done(mut self) {
        // Stop first: a download still writing would race the cleanup.
        self.manager.shutdown();
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

fn open(root: &Path, configure: impl FnOnce(&mut Settings)) -> Manager {
    let manager = Manager::open_with(&root.join("data"), Secrets::in_memory(), || {}).unwrap();
    let mut settings = manager.settings();
    settings.download_dir = root.join("downloads");
    settings.net.proxy.mode = ProxyMode::Direct;
    settings.connections = 4;
    settings.keep_awake = false;
    configure(&mut settings);
    manager.set_settings(settings).unwrap();
    manager.set_tuning_for_tests(Tuning {
        connections: 4,
        steal: StealPolicy { min_split: 64 * 1024, min_time: Duration::ZERO },
        checkpoint_every: Duration::from_millis(80),
        retry_limit: 3,
        backoff_base: Duration::from_millis(15),
        backoff_cap: Duration::from_millis(100),
        flush_bytes: 64 * 1024,
        flush_every: Duration::from_millis(40),
        max_request: None,
    });
    manager
}

fn completed(view: &DownloadView) -> bool {
    view.status == Status::Completed
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    names.sort();
    names
}

// ------------------------------------------------------- instant start ----

#[test]
fn a_captured_link_is_already_downloading_while_the_user_decides() {
    let fx = Fixture::new("instant");
    fx.server.route("/setup.dmg", Route::pattern(3 * MIB, 21).throttle(3 * MIB));
    let (_, provisional) = fx.capture("/setup.dmg");
    let id = provisional.expect("instant start is on by default");

    assert!(fx.manager.snapshot().is_empty(), "it is not in the list until the user says yes");
    let early = fx.wait(id, "bytes before any answer", |v| v.downloaded > 256 * 1024);
    assert!(early.provisional);
    assert_eq!(early.file_name, "setup.dmg");
    assert_eq!(early.total, Some(3 * MIB));
    assert!(fx.manager.drain_events().iter().all(|e| !matches!(e, Event::Completed { .. })));

    // "Download", with nothing changed: it simply carries on.
    fx.manager.confirm(id, Confirmation::default()).unwrap();
    let listed = fx.manager.snapshot();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].downloaded >= early.downloaded, "nothing was thrown away");
    let done = fx.wait(id, "completion", completed);
    assert!(!done.provisional);
    verify_file(&fx.downloads().join("setup.dmg"), 21, 3 * MIB).unwrap();
    let mut seen = Vec::new();
    fx.event(&mut seen, "the completion", |e| matches!(e, Event::Completed { id: finished, .. } if *finished == id));
    // One probe and the ranges: the file was not fetched twice.
    assert!(fx.server.stats("/setup.dmg").bytes_sent < 3 * MIB + 512 * 1024);
    fx.done();
}

#[test]
fn choosing_another_folder_and_name_moves_it_without_restarting() {
    let fx = Fixture::new("instant-move");
    fx.server.route("/data.bin", Route::pattern(4 * MIB, 22).throttle(3 * MIB));
    let (_, provisional) = fx.capture("/data.bin");
    let id = provisional.unwrap();
    let before = fx.wait(id, "some progress", |v| v.downloaded > 512 * 1024);

    let elsewhere = fx.root.join("elsewhere");
    fx.manager.confirm(id, Confirmation { dir: Some(elsewhere.clone()), file_name: Some("renamed.bin".into()), ..Default::default() }).unwrap();

    // The list shows where it is going at once; the bytes follow at the end.
    let during = fx.manager.get(id).unwrap();
    assert_eq!((during.dir.clone(), during.file_name.as_str()), (elsewhere.clone(), "renamed.bin"));
    if !completed(&during) {
        assert_eq!(during.staged_at, Some(fx.downloads().join("data.bin")));
    }
    assert!(during.downloaded >= before.downloaded);

    let done = fx.wait(id, "completion", completed);
    assert_eq!(done.staged_at, None);
    verify_file(&elsewhere.join("renamed.bin"), 22, 4 * MIB).unwrap();
    assert!(files_in(&fx.downloads()).is_empty(), "nothing is left behind: {:?}", files_in(&fx.downloads()));
    // It was never interrupted, so the server sent the file about once.
    assert!(fx.server.stats("/data.bin").bytes_sent < 4 * MIB + 512 * 1024);
    let mut seen = Vec::new();
    match fx.event(&mut seen, "the completion", |e| matches!(e, Event::Completed { .. })) {
        Event::Completed { path, name, .. } => {
            assert_eq!(path, elsewhere.join("renamed.bin"));
            assert_eq!(name, "renamed.bin");
        }
        _ => unreachable!(),
    }
    fx.done();
}

#[test]
fn a_file_that_finished_before_the_answer_is_moved_on_yes_and_deleted_on_no() {
    let fx = Fixture::new("instant-finished");
    fx.server.route("/small.zip", Route::pattern(200 * 1024, 23));
    fx.server.route("/other.zip", Route::pattern(200 * 1024, 24));

    // Yes, into another folder.
    let (_, provisional) = fx.capture("/small.zip");
    let id = provisional.unwrap();
    fx.wait(id, "completion behind the dialog", completed);
    assert!(fx.manager.snapshot().is_empty());
    assert!(fx.manager.drain_events().iter().all(|e| !matches!(e, Event::Completed { .. })), "not announced before the user's yes");
    let chosen = fx.root.join("chosen");
    fx.manager.confirm(id, Confirmation { dir: Some(chosen.clone()), ..Default::default() }).unwrap();
    let mut seen = Vec::new();
    fx.event(&mut seen, "the completion after confirming", |e| matches!(e, Event::Completed { path, .. } if path == &chosen.join("small.zip")));
    verify_file(&chosen.join("small.zip"), 23, 200 * 1024).unwrap();
    assert_eq!(fx.manager.snapshot().len(), 1);

    // No.
    let (_, provisional) = fx.capture("/other.zip");
    let id = provisional.unwrap();
    fx.wait(id, "completion behind the dialog", completed);
    assert_eq!(files_in(&fx.downloads()), vec!["other.zip"]);
    fx.manager.discard(id);
    assert!(fx.manager.get(id).is_none());
    assert!(files_in(&fx.downloads()).is_empty());
    assert_eq!(fx.manager.snapshot().len(), 1);
    fx.done();
}

#[test]
fn declining_or_quitting_leaves_no_trace_of_an_unconfirmed_download() {
    let fx = Fixture::new("instant-no");
    fx.server.route("/big.iso", Route::pattern(8 * MIB, 25).throttle(2 * MIB));

    let (_, provisional) = fx.capture("/big.iso");
    let id = provisional.unwrap();
    fx.wait(id, "some progress", |v| v.downloaded > 256 * 1024);
    assert!(part_path(&fx.downloads().join("big.iso")).exists());
    fx.manager.discard(id);
    let deadline = Instant::now() + Duration::from_secs(10);
    while fx.manager.get(id).is_some() || !files_in(&fx.downloads()).is_empty() {
        assert!(Instant::now() < deadline, "left behind: {:?}", files_in(&fx.downloads()));
        std::thread::sleep(Duration::from_millis(10));
    }

    // Quitting with the question still open.
    let (_, provisional) = fx.capture("/big.iso");
    let id = provisional.unwrap();
    fx.wait(id, "some progress", |v| v.downloaded > 256 * 1024);
    let fx = fx.reopen(|_| {});
    assert!(fx.manager.get(id).is_none());
    assert!(fx.manager.snapshot().is_empty());
    assert!(files_in(&fx.downloads()).is_empty(), "left behind: {:?}", files_in(&fx.downloads()));
    fx.done();
}

#[test]
fn instant_start_can_be_switched_off_and_never_doubles_a_download() {
    let fx = Fixture::with("instant-off", |s| s.start_immediately = false);
    fx.server.route("/a.bin", Route::pattern(MIB, 26).throttle(MIB));
    let (_, provisional) = fx.capture("/a.bin");
    assert_eq!(provisional, None);
    assert_eq!(fx.server.stats("/a.bin").requests, 0, "nothing is requested before the user's yes");

    let mut settings = fx.manager.settings();
    settings.start_immediately = true;
    fx.manager.set_settings(settings).unwrap();
    // Already in the list: the dialog will ask, nothing starts a second copy.
    let id = fx.manager.add(NewDownload::new(Request::new(fx.server.url("/a.bin"))));
    let (_, provisional) = fx.capture("/a.bin");
    assert_eq!(provisional, None);
    let duplicate = fx.manager.duplicate_of(&fx.server.url("/a.bin")).unwrap();
    assert_eq!(duplicate.id, id);
    assert!(!duplicate.file_exists);
    fx.wait(id, "completion", completed);
    assert!(fx.manager.duplicate_of(&fx.server.url("/a.bin")).unwrap().file_exists);
    assert!(fx.manager.duplicate_of(&fx.server.url("/b.bin")).is_none());
    fx.done();
}

// --------------------------------------------------------------- queue ----

#[test]
fn downloads_set_aside_wait_for_the_queue_and_run_in_order() {
    let fx = Fixture::with("queue", |s| {
        s.max_active = 1;
        s.schedule.when_done = WhenDone::Sleep;
    });
    let ids: Vec<DownloadId> = (0..3)
        .map(|n| {
            let path = format!("/later-{n}.bin");
            fx.server.route(&path, Route::pattern(300 * 1024, 30 + n));
            fx.manager.add(NewDownload { request: Request::new(fx.server.url(&path)), later: true, ..Default::default() })
        })
        .collect();

    std::thread::sleep(Duration::from_millis(300));
    for id in &ids {
        assert_eq!(fx.manager.get(*id).unwrap().status, Status::Scheduled);
    }
    assert_eq!(fx.server.stats("/later-0.bin").requests, 0);
    let queue = fx.manager.queue();
    assert_eq!((queue.running, queue.waiting), (false, 3));
    // "Resume all" is about what the user paused, not about the queue.
    fx.manager.resume_all();
    assert_eq!(fx.manager.get(ids[0]).unwrap().status, Status::Scheduled);

    assert_eq!(fx.manager.start_queue(), 3);
    assert!(fx.manager.queue().running);
    let mut seen = Vec::new();
    fx.event(&mut seen, "queue start", |e| matches!(e, Event::QueueStarted { count: 3, by_schedule: false }));
    for (n, id) in ids.iter().enumerate() {
        fx.wait(*id, "completion", completed);
        verify_file(&fx.downloads().join(format!("later-{n}.bin")), 30 + n as u64, 300 * 1024).unwrap();
    }
    match fx.event(&mut seen, "queue end", |e| matches!(e, Event::QueueFinished { .. })) {
        Event::QueueFinished { when_done, completed, failed } => assert_eq!((when_done, completed, failed), (WhenDone::Sleep, 3, 0)),
        _ => unreachable!(),
    }
    let finished: Vec<u64> = ids.iter().map(|id| fx.manager.get(*id).unwrap().finished_at.unwrap()).collect();
    assert!(finished.windows(2).all(|w| w[0] <= w[1]), "ran oldest first: {finished:?}");
    assert!(!fx.manager.queue().running);
    assert_eq!(fx.manager.start_queue(), 0, "an empty queue does not start");
    fx.done();
}

#[test]
fn stopping_the_queue_puts_its_downloads_back_with_their_data() {
    let fx = Fixture::new("queue-stop");
    fx.server.route("/slow.bin", Route::pattern(4 * MIB, 40).throttle(2 * MIB));
    fx.server.route("/mine.bin", Route::pattern(4 * MIB, 41).throttle(2 * MIB));
    let queued = fx.manager.add(NewDownload { request: Request::new(fx.server.url("/slow.bin")), later: true, ..Default::default() });
    // A download the user started themselves is not the queue's to stop.
    let own = fx.manager.add(NewDownload::new(Request::new(fx.server.url("/mine.bin"))));

    fx.manager.start_queue();
    fx.wait(queued, "progress", |v| v.downloaded > 512 * 1024);
    assert!(fx.manager.get(queued).unwrap().queue_run);
    assert_eq!(fx.manager.stop_queue(), 1);
    let held = fx.wait(queued, "back in the queue", |v| v.status == Status::Scheduled);
    assert!(held.downloaded > 512 * 1024, "what it fetched is kept");
    assert!(meta_path(&held.path()).exists());
    assert!(fx.manager.get(own).unwrap().status.is_active());
    assert!(fx.manager.drain_events().iter().all(|e| !matches!(e, Event::QueueFinished { .. })), "stopped by hand: no end-of-queue action");

    // Download later, on something already running.
    fx.manager.schedule(&[own]);
    let held_own = fx.wait(own, "set aside", |v| v.status == Status::Scheduled);
    assert!(held_own.downloaded > 0);

    assert_eq!(fx.manager.start_queue(), 2);
    for (id, seed, name) in [(queued, 40, "slow.bin"), (own, 41, "mine.bin")] {
        fx.wait(id, "completion", completed);
        verify_file(&fx.downloads().join(name), seed, 4 * MIB).unwrap();
    }
    fx.done();
}

#[test]
fn download_later_on_a_captured_link_keeps_what_instant_start_fetched() {
    let fx = Fixture::new("queue-capture");
    fx.server.route("/night.iso", Route::pattern(4 * MIB, 42).throttle(2 * MIB));
    let (_, provisional) = fx.capture("/night.iso");
    let id = provisional.unwrap();
    fx.wait(id, "progress", |v| v.downloaded > 512 * 1024);
    fx.manager.confirm(id, Confirmation { later: true, ..Default::default() }).unwrap();
    let held = fx.wait(id, "set aside", |v| v.status == Status::Scheduled);
    assert!(!held.provisional);
    assert!(held.downloaded > 512 * 1024);
    assert_eq!(fx.manager.snapshot().len(), 1);
    fx.manager.start_queue();
    fx.wait(id, "completion", completed);
    verify_file(&fx.downloads().join("night.iso"), 42, 4 * MIB).unwrap();
    fx.done();
}

#[test]
fn the_queue_starts_by_itself_when_opened_inside_its_window() {
    let fx = Fixture::new("queue-window");
    fx.server.route("/scheduled.bin", Route::pattern(300 * 1024, 43));
    let id = fx.manager.add(NewDownload { request: Request::new(fx.server.url("/scheduled.bin")), later: true, ..Default::default() });
    assert_eq!(fx.manager.get(id).unwrap().status, Status::Scheduled);

    // A window that opened a moment ago and closes in an hour.
    let now = LocalTime::now().minute;
    let fx = fx.reopen(|s| {
        s.schedule.enabled = true;
        s.schedule.start = (now + 1440 - 1) % 1440;
        s.schedule.stop_enabled = true;
        s.schedule.stop = (now + 60) % 1440;
        s.schedule.days = [true; 7];
    });
    // `reopen` applies the settings after opening, so open once more: this
    // time the schedule is what was saved.
    let fx = fx.reopen(|_| {});
    fx.wait(id, "completion without anyone starting the queue", completed);
    verify_file(&fx.downloads().join("scheduled.bin"), 43, 300 * 1024).unwrap();
    assert!(fx.manager.queue().next_start_in.is_some());
    fx.done();
}

// ---------------------------------------------------------- site rules ----

#[test]
fn a_site_rule_limits_connections_and_supplies_the_login() {
    let fx = Fixture::new("sites");
    // base64("ada:correct horse")
    let encoded = "YWRhOmNvcnJlY3QgaG9yc2U=";
    fx.server.route("/private.bin", Route::pattern(2 * MIB, 50).throttle(4 * MIB).basic_auth(encoded));
    let url = fx.server.url("/private.bin");

    // Without a login the server says no, and the download waits.
    let id = fx.manager.add(NewDownload::new(Request::new(url.clone())));
    fx.wait(id, "the refusal", |v| matches!(v.status, Status::NeedsRefresh { code: 401 }));
    let probe = fx.manager.probe(Request::new(url.clone()));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !probe.is_ready() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(matches!(probe.take(), Some(Err(Error::Expired(401)))));

    // The user enters the login for the site; one connection only.
    fx.manager.save_login("127.0.0.1", "ada", "correct horse").unwrap();
    let mut settings = fx.manager.settings();
    assert_eq!(settings.sites.len(), 1);
    settings.sites[0].connections = Some(1);
    fx.manager.set_settings(settings).unwrap();
    assert!(!std::fs::read_to_string(fx.root.join("data/settings.json")).unwrap().contains("correct horse"), "the password is not in the settings file");

    fx.manager.resume(id);
    let done = fx.wait(id, "completion", completed);
    verify_file(&done.path(), 50, 2 * MIB).unwrap();
    let stats = fx.server.stats("/private.bin");
    assert_eq!(stats.peak_connections, 1, "the site rule allows one connection");
    assert_eq!(stats.last_headers.get("authorization").map(String::as_str), Some(format!("Basic {encoded}").as_str()));

    // A site without a rule gets neither the login nor the limit.
    fx.manager.set_settings(Settings { sites: vec![SiteRule { pattern: "other.example".into(), connections: Some(1), username: "ada".into() }], ..fx.manager.settings() }).unwrap();
    fx.server.route("/open.bin", Route::pattern(2 * MIB, 51).throttle(2 * MIB));
    let open = fx.manager.add(NewDownload::new(Request::new(fx.server.url("/open.bin"))));
    fx.wait(open, "completion", completed);
    let stats = fx.server.stats("/open.bin");
    assert!(stats.peak_connections > 1);
    assert!(!stats.last_headers.contains_key("authorization"));
    fx.done();
}

// ------------------------------------------------------------- sorting ----

#[test]
fn files_are_sorted_into_folders_by_what_the_server_calls_them() {
    let fx = Fixture::with("sort", |s| s.sort_by_type = true);
    fx.server.route("/movie.mkv", Route::pattern(100 * 1024, 60));
    // The address says nothing; the server names the file.
    fx.server.route("/get", Route::pattern(100 * 1024, 61).disposition("attachment; filename=\"report.pdf\""));
    fx.server.route("/mystery", Route::pattern(100 * 1024, 62));

    let movie = fx.manager.add(NewDownload::new(Request::new(fx.server.url("/movie.mkv"))));
    let report = fx.manager.add(NewDownload::new(Request::new(fx.server.url("/get?id=7"))));
    let mystery = fx.manager.add(NewDownload::new(Request::new(fx.server.url("/mystery"))));
    // The user's explicit choice is never second-guessed.
    let pinned = fx.manager.add(NewDownload { request: Request::new(fx.server.url("/movie.mkv")), dir: Some(fx.root.join("pinned")), ..Default::default() });

    assert_eq!(fx.wait(movie, "completion", completed).path(), fx.downloads().join("Video/movie.mkv"));
    assert_eq!(fx.wait(report, "completion", completed).path(), fx.downloads().join("Documents/report.pdf"));
    assert_eq!(fx.wait(mystery, "completion", completed).dir, fx.downloads());
    assert_eq!(fx.wait(pinned, "completion", completed).path(), fx.root.join("pinned/movie.mkv"));
    verify_file(&fx.downloads().join("Documents/report.pdf"), 61, 100 * 1024).unwrap();

    // A captured link is fetched straight into its family's folder.
    let (_, provisional) = fx.capture("/get?id=8");
    let id = provisional.unwrap();
    fx.wait(id, "completion", completed);
    fx.manager.confirm(id, Confirmation::default()).unwrap();
    assert_eq!(fx.manager.get(id).unwrap().dir, fx.downloads().join("Documents"));
    fx.done();
}

// ----------------------------------------------------------- finishing ----

#[test]
fn finished_files_get_their_labels_date_and_command() {
    let marker = std::env::temp_dir().join(format!("siphon-flow-{}-marker.txt", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let command = format!("basename [File] > '{}'", marker.display());
    let fx = Fixture::with("finishing", |s| {
        s.keep_server_date = true;
        s.on_complete = command.clone();
    });
    fx.server.route("/tool.dmg", Route::pattern(200 * 1024, 70).last_modified("Wed, 21 Oct 2015 07:28:00 GMT"));
    let id = fx.manager.add(NewDownload::new(Request::new(fx.server.url("/tool.dmg")).with_referer("https://site.example/tools")));
    let done = fx.wait(id, "completion", completed);

    let modified = std::fs::metadata(done.path()).unwrap().modified().unwrap();
    assert_eq!(modified.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(), 1_445_412_480);
    #[cfg(target_os = "macos")]
    {
        let quarantine = String::from_utf8(xattr::get(done.path(), "com.apple.quarantine").unwrap().expect("quarantined")).unwrap();
        assert!(quarantine.starts_with("0083;") && quarantine.contains(";Siphon;"), "{quarantine}");
        let sources: Vec<String> = plist::from_bytes(&xattr::get(done.path(), "com.apple.metadata:kMDItemWhereFroms").unwrap().unwrap()).unwrap();
        assert_eq!(sources, vec![fx.server.url("/tool.dmg"), "https://site.example/tools".to_owned()]);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::read_to_string(&marker).map(|s| s.trim() != "tool.dmg").unwrap_or(true) {
        assert!(Instant::now() < deadline, "the command did not run");
        std::thread::sleep(Duration::from_millis(20));
    }

    // Switched off: none of it happens.
    let mut settings = fx.manager.settings();
    settings.mark_downloaded = false;
    settings.keep_server_date = false;
    settings.on_complete.clear();
    fx.manager.set_settings(settings).unwrap();
    std::fs::remove_file(&marker).unwrap();
    fx.server.route("/plain.bin", Route::pattern(50 * 1024, 71).last_modified("Wed, 21 Oct 2015 07:28:00 GMT"));
    let plain = fx.manager.add(NewDownload::new(Request::new(fx.server.url("/plain.bin"))));
    let done = fx.wait(plain, "completion", completed);
    #[cfg(target_os = "macos")]
    assert_eq!(xattr::get(done.path(), "com.apple.quarantine").unwrap(), None);
    assert!(std::fs::metadata(done.path()).unwrap().modified().unwrap().elapsed().unwrap() < Duration::from_secs(120));
    std::thread::sleep(Duration::from_millis(200));
    assert!(!marker.exists());
    fx.done();
}

// ----------------------------------------------------------- checksums ----

#[test]
fn a_finished_file_can_be_checked_against_its_published_checksum() {
    let fx = Fixture::new("checksum");
    fx.server.route("/image.iso", Route::pattern(3 * MIB + 123, 80).throttle(8 * MIB));
    let id = fx.manager.add(NewDownload::new(Request::new(fx.server.url("/image.iso"))));
    assert!(fx.manager.checksum(id, HashAlgorithm::Sha256).is_none(), "only finished files can be checked");
    fx.wait(id, "completion", completed);

    let expected: String = Sha256::digest(pattern(80, 3 * MIB + 123)).iter().map(|b| format!("{b:02x}")).collect();
    let job = fx.manager.checksum(id, HashAlgorithm::Sha256).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let computed = loop {
        if let Some(result) = job.take() {
            break result.unwrap();
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(computed, expected);
    assert_eq!(job.fraction(), 1.0);
    assert!(siphon_core::checksum::matches(&computed, &format!("SHA256: {}", expected.to_uppercase())));
    fx.done();
}

// ------------------------------------------------------- archive peek ----

#[test]
fn a_remote_zip_is_listed_from_two_small_requests() {
    let fx = Fixture::new("zip");
    let payload = vec![7u8; 900 * 1024];
    let zip = siphon_core::zip_peek::fixture::build(&[("docs/", b""), ("docs/manual.pdf", &payload), ("readme.txt", b"hello"), ("großes bild.png", &[1u8; 2048])], b"built for a test", false);
    let size = zip.len() as u64;
    fx.server.route("/bundle.zip", Route::bytes(zip.clone(), "application/zip"));
    fx.server.route("/bundle64.zip", Route::bytes(siphon_core::zip_peek::fixture::build(&[("a.bin", &payload)], b"", true), "application/zip"));
    fx.server.route("/whole.zip", Route::bytes(zip, "application/zip").no_ranges());
    fx.server.route("/fake.zip", Route::pattern(300 * 1024, 90));

    let peek = |path: &str| {
        let ticket = fx.manager.peek_archive(Request::new(fx.server.url(path)));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(result) = ticket.take() {
                return result;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    };

    let listing = peek("/bundle.zip").unwrap();
    let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["docs/", "docs/manual.pdf", "readme.txt", "großes bild.png"]);
    assert_eq!(listing.files, 3);
    assert_eq!(listing.unpacked, 900 * 1024 + 5 + 2048);
    assert!(!listing.truncated);
    // The point of the exercise: the archive itself was not downloaded.
    let sent = fx.server.stats("/bundle.zip").bytes_sent;
    assert!(sent < size / 4, "{sent} of {size} bytes were fetched");

    assert_eq!(peek("/bundle64.zip").unwrap().entries[0].size, 900 * 1024);
    assert!(matches!(peek("/whole.zip"), Err(Error::Unsupported(_))));
    assert!(matches!(peek("/fake.zip"), Err(Error::Protocol(_))));
    fx.done();
}

// -------------------------------------------------------- link grabber ----

#[test]
fn a_page_and_the_pages_under_it_are_explored_for_files() {
    let fx = Fixture::new("grab");
    let html = "text/html; charset=utf-8";
    fx.server.route(
        "/site/index.html",
        Route::bytes(
            r#"<title>Downloads</title>
               <a href="one.zip">One</a> <a href="/site/sub/page.html">More</a> <a href="chapter2.html">Chapter 2</a>
               <a href="https://elsewhere.example/far.html">Away</a> <img src="logo.png" alt="Logo">"#,
            html,
        ),
    );
    fx.server.route("/site/sub/page.html", Route::bytes(r#"<a href="two.iso">Two</a> <a href="deeper.html">Deeper</a> <a href="../one.zip">One again</a>"#, html));
    fx.server.route("/site/chapter2.html", Route::bytes(r#"<video><source src="clip.mp4"></video>"#, html));
    fx.server.route("/site/sub/deeper.html", Route::bytes(r#"<a href="three.pdf">Three</a>"#, html));
    fx.server.route("/site/one.zip", Route::pattern(1024, 95));

    let explore = |depth: u8| {
        let job = fx.manager.explore(Request::new(fx.server.url("/site/index.html")), GrabOptions { depth, ..Default::default() });
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(result) = job.take() {
                return (result, job.progress.pages_read.load(std::sync::atomic::Ordering::Relaxed));
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let names = |batch: &siphon_core::grab::LinkBatch, kind: LinkKind| -> Vec<String> {
        let mut out: Vec<String> = batch.links.iter().filter(|l| l.kind == kind).map(|l| l.url.rsplit('/').next().unwrap().to_owned()).collect();
        out.sort();
        out
    };

    let (page, read) = explore(0);
    let page = page.unwrap();
    assert_eq!(read, 1);
    assert_eq!(page.page_title, "Downloads");
    assert_eq!(names(&page, LinkKind::File), vec!["one.zip"]);
    assert_eq!(names(&page, LinkKind::Embedded), vec!["logo.png"]);
    assert_eq!(page.links.iter().find(|l| l.url.ends_with("one.zip")).unwrap().text, "One");

    let (one, read) = explore(1);
    let one = one.unwrap();
    assert_eq!(read, 3, "the page and the two pages it links to on this site");
    assert_eq!(names(&one, LinkKind::File), vec!["one.zip", "two.iso"]);
    assert_eq!(names(&one, LinkKind::Embedded), vec!["clip.mp4", "logo.png"]);
    assert_eq!(fx.server.stats("/site/sub/deeper.html").requests, 0);

    let (two, read) = explore(2);
    assert_eq!(read, 4);
    assert_eq!(names(&two.unwrap(), LinkKind::File), vec!["one.zip", "three.pdf", "two.iso"]);

    // The request for a found link carries the page as its referrer.
    let request = one.request_for(&fx.server.url("/site/one.zip"));
    assert_eq!(request.referer.as_deref(), Some(fx.server.url("/site/index.html").as_str()));

    // A file is not a page; a missing page says so.
    let job = fx.manager.explore(Request::new(fx.server.url("/site/one.zip")), GrabOptions::default());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !matches!(job.take(), Some(Err(Error::Unsupported(_)))) {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let job = fx.manager.explore(Request::new(fx.server.url("/site/nothing.html")), GrabOptions::default());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !matches!(job.take(), Some(Err(Error::Expired(404)))) {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    fx.done();
}

// --------------------------------------------------------- fresh links ----

#[test]
fn a_download_waiting_for_its_fresh_link_takes_the_next_capture() {
    let fx = Fixture::new("await");
    let size = 6 * MIB;
    fx.server.route("/file", Route::pattern(size, 96).throttle(2 * MIB).token("a").drop_after(700 * 1024).disposition("attachment; filename=\"archive.7z\""));
    let id = fx.manager.add(NewDownload::new(Request::new(fx.server.url("/file?token=a"))));
    fx.wait(id, "some progress", |v| v.downloaded > MIB);
    fx.server.update("/file", |r| r.token = Some("b".into()));
    fx.wait(id, "expiry", |v| matches!(v.status, Status::NeedsRefresh { .. }));
    fx.manager.drain_events();

    // "Refresh link": the app opens the page and waits. The capture that
    // arrives carries no size and no name, so nothing but the wait ties it
    // to this download; the size check still protects the data.
    fx.manager.await_fresh_link(id);
    assert_eq!(fx.manager.awaiting(), Some(id));
    fx.manager.capture(Capture { request: Request::new(fx.server.url("/file?token=b")), ..Default::default() });
    let mut seen = Vec::new();
    fx.event(&mut seen, "the revival", |e| matches!(e, Event::AutoRefreshed { id: revived, .. } if *revived == id));
    assert_eq!(fx.manager.awaiting(), None);
    assert!(seen.iter().all(|e| !matches!(e, Event::Captured { .. })), "no question was asked");
    let done = fx.wait(id, "completion", completed);
    verify_file(&done.path(), 96, size).unwrap();
    assert_eq!(fx.manager.snapshot().len(), 1);

    // A link to a different file is not swallowed: it is asked about as usual.
    fx.server.route("/unrelated.bin", Route::pattern(100 * 1024, 97));
    fx.server.route("/stalled", Route::pattern(size, 98).throttle(2 * MIB).token("a").drop_after(700 * 1024));
    let stalled = fx.manager.add(NewDownload::new(Request::new(fx.server.url("/stalled?token=a"))));
    fx.wait(stalled, "some progress", |v| v.downloaded > MIB);
    fx.server.update("/stalled", |r| r.token = Some("b".into()));
    fx.wait(stalled, "expiry", |v| matches!(v.status, Status::NeedsRefresh { .. }));
    fx.manager.await_fresh_link(stalled);
    fx.manager.capture(Capture { request: Request::new(fx.server.url("/unrelated.bin")), ..Default::default() });
    let mut seen = Vec::new();
    fx.event(&mut seen, "the ordinary capture", |e| matches!(e, Event::Captured { .. }));
    assert_eq!(fx.manager.awaiting(), Some(stalled), "it keeps waiting");
    assert!(matches!(fx.manager.get(stalled).unwrap().status, Status::NeedsRefresh { .. }));
    fx.done();
}

// --------------------------------------------------------------- batch ----

#[test]
fn many_downloads_are_added_in_order_and_exported() {
    let fx = Fixture::with("batch", |s| s.max_active = 1);
    let (links, problem) = siphon_core::batch::collect(&format!("{}/part[1-4].bin", fx.server.url("")));
    assert_eq!(problem, None);
    assert_eq!(links.len(), 4);
    for n in 1..=4u64 {
        fx.server.route(&format!("/part{n}.bin"), Route::pattern(150 * 1024, 100 + n));
    }
    let ids = fx.manager.add_many(links.iter().map(|l| NewDownload::new(Request::new(l.clone()))).collect());
    assert_eq!(ids.len(), 4);
    for (n, id) in ids.iter().enumerate() {
        let done = fx.wait(*id, "completion", completed);
        assert_eq!(done.file_name, format!("part{}.bin", n + 1));
    }
    let finished: Vec<u64> = ids.iter().map(|id| fx.manager.get(*id).unwrap().finished_at.unwrap()).collect();
    assert!(finished.windows(2).all(|w| w[0] <= w[1]));

    assert_eq!(fx.manager.export_links(&[]).lines().collect::<Vec<_>>(), links.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(fx.manager.export_links(&[ids[2]]).trim(), links[2]);

    // A second capture of an address that is already on its way is not a second download.
    let mut settings = fx.manager.settings();
    settings.save_automatically = true;
    fx.manager.set_settings(settings).unwrap();
    fx.server.route("/again.bin", Route::pattern(4 * MIB, 110).throttle(2 * MIB));
    let running = fx.manager.add(NewDownload::new(Request::new(fx.server.url("/again.bin"))));
    fx.wait(running, "progress", |v| v.downloaded > 0);
    fx.manager.capture(Capture { request: Request::new(fx.server.url("/again.bin")), ..Default::default() });
    let mut seen = Vec::new();
    fx.event(&mut seen, "the duplicate notice", |e| matches!(e, Event::AlreadyDownloading { id, .. } if *id == running));
    assert_eq!(fx.manager.snapshot().len(), 5);
    fx.done();
}

// ---------------------------------------------------------- keep awake ----

#[cfg(target_os = "macos")]
#[test]
fn the_computer_is_kept_awake_only_while_something_is_downloading() {
    fn helpers() -> usize {
        let out = std::process::Command::new("/usr/bin/pgrep").args(["-P", &std::process::id().to_string(), "-x", "caffeinate"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).lines().count()
    }
    let fx = Fixture::with("awake", |s| s.keep_awake = true);
    fx.server.route("/long.bin", Route::pattern(12 * MIB, 120).throttle(MIB));
    let before = helpers();
    let id = fx.manager.add(NewDownload::new(Request::new(fx.server.url("/long.bin"))));
    let deadline = Instant::now() + Duration::from_secs(10);
    while helpers() <= before {
        assert!(Instant::now() < deadline, "no sleep hold was taken while downloading");
        std::thread::sleep(Duration::from_millis(50));
    }
    fx.manager.pause(id);
    let deadline = Instant::now() + Duration::from_secs(10);
    while helpers() > before {
        assert!(Instant::now() < deadline, "the sleep hold outlived the download");
        std::thread::sleep(Duration::from_millis(50));
    }

    // Switched off: no hold at all.
    let mut settings = fx.manager.settings();
    settings.keep_awake = false;
    fx.manager.set_settings(settings).unwrap();
    fx.manager.resume(id);
    fx.wait(id, "progress", |v| v.status == Status::Downloading);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(helpers(), before);
    fx.done();
}

// ------------------------------------------------------- the real clock ----

/// Waits for the wall clock to reach the scheduled minute, so it runs only
/// when asked for: `cargo test -p siphon-core --test workflow clock -- --ignored`.
#[test]
#[ignore = "waits up to a minute for the wall clock"]
fn the_schedule_starts_and_stops_the_queue_by_the_clock() {
    let fx = Fixture::new("clock");
    // Large and slow enough to still be running when the stop time comes.
    fx.server.route("/nightly.bin", Route::pattern(600 * MIB, 130).throttle(MIB / 4));
    let id = fx.manager.add(NewDownload { request: Request::new(fx.server.url("/nightly.bin")), later: true, ..Default::default() });

    let now = LocalTime::now().minute;
    let mut settings = fx.manager.settings();
    settings.schedule.enabled = true;
    settings.schedule.start = (now + 1) % 1440;
    settings.schedule.stop_enabled = true;
    settings.schedule.stop = (now + 2) % 1440;
    settings.schedule.days = [true; 7];
    fx.manager.set_settings(settings).unwrap();
    assert_eq!(fx.manager.get(id).unwrap().status, Status::Scheduled);
    let waiting = fx.manager.queue().next_start_in.expect("a start is coming");
    assert!(waiting <= 1, "starts within the minute, not in {waiting} minutes");

    // The start minute arrives.
    let deadline = Instant::now() + Duration::from_secs(75);
    let mut seen = Vec::new();
    loop {
        seen.extend(fx.manager.drain_events());
        if seen.iter().any(|e| matches!(e, Event::QueueStarted { count: 1, by_schedule: true })) {
            break;
        }
        assert!(Instant::now() < deadline, "the queue did not start on schedule");
        std::thread::sleep(Duration::from_millis(100));
    }
    fx.wait(id, "progress under the schedule", |v| v.downloaded > 0 && v.queue_run);

    // A minute later, the stop time puts it back, with its data.
    let deadline = Instant::now() + Duration::from_secs(75);
    loop {
        let view = fx.manager.get(id).unwrap();
        if view.status == Status::Scheduled {
            assert!(view.downloaded > 0);
            break;
        }
        assert!(Instant::now() < deadline, "the queue did not stop on schedule; state {:?}", view.status);
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!fx.manager.queue().running);
    fx.done();
}

// ------------------------------------------- instant start, every path ----

/// A finished (VOD) media playlist of `count` segments under `/{name}/`.
fn serve_stream(server: &TestServer, name: &str, count: usize, segment: u64, throttle: u64) -> String {
    let mut playlist = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n");
    for i in 0..count {
        playlist.push_str(&format!("#EXTINF:6.0,\nseg{i}.ts\n"));
        server.route(&format!("/{name}/seg{i}.ts"), Route::pattern(segment, 900 + i as u64).content_type("video/mp2t").throttle(throttle));
    }
    playlist.push_str("#EXT-X-ENDLIST\n");
    server.route(&format!("/{name}/index.m3u8"), Route::bytes(playlist, "application/vnd.apple.mpegurl"));
    server.url(&format!("/{name}/index.m3u8"))
}

#[test]
fn a_stream_is_fetched_ahead_too_and_lands_under_its_real_container() {
    let fx = Fixture::new("instant-stream");
    let url = serve_stream(&fx.server, "talk", 6, 300 * 1024, 600 * 1024);
    let capture = Capture { request: Request::new(url).with_referer("https://site.example/talk"), suggested_name: Some("Talk.mp4".into()), media: true, ..Default::default() };
    fx.manager.capture(capture);
    let mut seen = Vec::new();
    let id = match fx.event(&mut seen, "the capture", |e| matches!(e, Event::Captured { .. })) {
        Event::Captured { provisional, .. } => provisional.expect("streams start ahead as well"),
        _ => unreachable!(),
    };
    let early = fx.wait(id, "progress before any answer", |v| v.downloaded > 0);
    assert!(early.provisional);
    assert_eq!(early.file_name, "Talk.mp4", "named by the page until the stream says otherwise");
    assert!(fx.manager.snapshot().is_empty());

    // Accepted while still running, under another name, in another folder.
    let chosen = fx.root.join("talks");
    fx.manager.confirm(id, Confirmation { dir: Some(chosen.clone()), file_name: Some("Lecture.mp4".into()), ..Default::default() }).unwrap();
    let done = fx.wait(id, "completion", completed);
    // The segments are not real video, so they cannot be remuxed: the file
    // is an MPEG transport stream and is named as one, in the chosen place.
    assert_eq!(done.path(), chosen.join("Lecture.ts"));
    assert_eq!(std::fs::metadata(done.path()).unwrap().len(), 6 * 300 * 1024);
    assert!(files_in(&fx.downloads()).is_empty(), "nothing is left behind: {:?}", files_in(&fx.downloads()));

    // Declined: the pieces fetched so far are deleted with it.
    let url = serve_stream(&fx.server, "other", 6, 300 * 1024, 300 * 1024);
    fx.manager.capture(Capture { request: Request::new(url), suggested_name: Some("Other.mp4".into()), media: true, ..Default::default() });
    let id = match fx.event(&mut seen, "the capture", |e| matches!(e, Event::Captured { .. })) {
        Event::Captured { provisional, .. } => provisional.unwrap(),
        _ => unreachable!(),
    };
    fx.wait(id, "progress", |v| v.downloaded > 0);
    fx.manager.discard(id);
    let deadline = Instant::now() + Duration::from_secs(10);
    while fx.manager.get(id).is_some() || !files_in(&fx.downloads()).is_empty() {
        assert!(Instant::now() < deadline, "left behind: {:?}", files_in(&fx.downloads()));
        std::thread::sleep(Duration::from_millis(10));
    }
    fx.done();
}

#[test]
fn a_link_being_asked_about_does_not_wait_for_a_free_slot() {
    let fx = Fixture::with("instant-slot", |s| s.max_active = 1);
    fx.server.route("/busy.bin", Route::pattern(40 * MIB, 140).throttle(MIB));
    fx.server.route("/asked.bin", Route::pattern(2 * MIB, 141).throttle(4 * MIB));
    fx.server.route("/patient.bin", Route::pattern(MIB, 142));
    let busy = fx.manager.add(NewDownload::new(Request::new(fx.server.url("/busy.bin"))));
    fx.wait(busy, "the only slot to be taken", |v| v.status == Status::Downloading);

    // An ordinary download waits its turn...
    let patient = fx.manager.add(NewDownload::new(Request::new(fx.server.url("/patient.bin"))));
    // ...the one the user is looking at does not.
    let (_, provisional) = fx.capture("/asked.bin");
    let asked = provisional.unwrap();
    fx.wait(asked, "the captured link to finish behind the dialog", completed);
    assert_eq!(fx.manager.get(busy).unwrap().status, Status::Downloading);
    assert_eq!(fx.manager.get(patient).unwrap().status, Status::Queued);
    assert!(fx.manager.drain_events().iter().all(|e| !matches!(e, Event::Completed { .. })), "nothing is announced before the user's yes");

    // Later, the user remembers the dialog and says yes: it is simply done.
    fx.manager.confirm(asked, Confirmation::default()).unwrap();
    let mut seen = Vec::new();
    fx.event(&mut seen, "the completion", |e| matches!(e, Event::Completed { id, .. } if *id == asked));
    verify_file(&fx.downloads().join("asked.bin"), 141, 2 * MIB).unwrap();
    fx.done();
}

#[test]
fn a_link_typed_by_hand_starts_ahead_under_the_same_rules() {
    let fx = Fixture::new("instant-typed");
    fx.server.route("/typed.bin", Route::pattern(MIB, 143));
    let capture = Capture { request: Request::new(fx.server.url("/typed.bin")), ..Default::default() };

    let id = fx.manager.start_ahead(&capture).expect("starts ahead");
    let done = fx.wait(id, "completion behind the dialog", completed);
    assert!(done.provisional);
    assert!(fx.manager.snapshot().is_empty());
    // The address was edited: the download behind it goes without a trace.
    fx.manager.discard(id);
    assert!(fx.manager.get(id).is_none());
    assert!(files_in(&fx.downloads()).is_empty());

    // Already in the list: the dialog asks first, nothing starts.
    let listed = fx.manager.add(NewDownload::new(Request::new(fx.server.url("/typed.bin"))));
    assert_eq!(fx.manager.start_ahead(&capture), None);
    fx.wait(listed, "completion", completed);

    // Switched off: never.
    let mut settings = fx.manager.settings();
    settings.start_immediately = false;
    fx.manager.set_settings(settings).unwrap();
    fx.server.route("/quiet.bin", Route::pattern(MIB, 144));
    assert_eq!(fx.manager.start_ahead(&Capture { request: Request::new(fx.server.url("/quiet.bin")), ..Default::default() }), None);
    assert_eq!(fx.server.stats("/quiet.bin").requests, 0);
    fx.done();
}

// --------------------------------------------------------- video pages ----

/// Stands in for the video helper: a script that prints whatever
/// description the test has put in `page.json`, or fails the way the real
/// one does when `error.txt` exists.
#[cfg(unix)]
fn fake_resolver(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).unwrap();
    let script = dir.join("resolver.sh");
    std::fs::write(
        &script,
        format!(
            concat!(
                "#!/bin/sh\n",
                "if [ -f '{0}/error.txt' ]; then cat '{0}/error.txt' >&2; exit 1; fi\n",
                "echo \"$@\" >> '{0}/calls.txt'\n",
                // With `login.txt` in place the page is only described to a
                // caller that brings cookies, as some sites insist.
                "if [ -f '{0}/login.txt' ]; then\n",
                "  jar=''; prev=''\n",
                "  for arg in \"$@\"; do if [ \"$prev\" = '--cookies' ]; then jar=\"$arg\"; fi; prev=\"$arg\"; done\n",
                "  if [ -z \"$jar\" ]; then echo 'ERROR: [Reddit] abc123: Account authentication is required. Use --cookies-from-browser or --cookies for the authentication. See  https://github.com/yt-dlp/yt-dlp/wiki/FAQ#how-do-i-pass-cookies-to-yt-dlp  for how to manually pass cookies' >&2; exit 1; fi\n",
                "  cp \"$jar\" '{0}/seen-cookies.txt'\n",
                "fi\n",
                "cat '{0}/page.json'\n",
            ),
            dir.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

#[allow(clippy::too_many_arguments)]
fn stream(id: &str, ext: &str, vcodec: &str, acodec: &str, height: Option<u32>, size: u64, url: &str, piece: u64) -> serde_json::Value {
    serde_json::json!({
        "format_id": id, "ext": ext, "protocol": "https", "vcodec": vcodec, "acodec": acodec, "height": height,
        "fps": 30, "tbr": 1000, "filesize": size, "url": url,
        "http_headers": { "User-Agent": "Resolver/1.0" },
        "downloader_options": { "http_chunk_size": piece },
        "language_preference": 10,
    })
}

#[cfg(unix)]
#[test]
fn a_video_page_is_resolved_fetched_in_pieces_and_joined() {
    let Some(ffmpeg) = siphon_core::hls::find_ffmpeg() else {
        eprintln!("skipped: ffmpeg is not installed");
        return;
    };
    let fx = Fixture::new("page");
    let tools = fx.root.join("tools");
    fx.manager.set_resolver_for_tests(fake_resolver(&tools));

    // A real picture stream and a real sound stream, two seconds each.
    let make = |args: &[&str], out: &Path| {
        let done = std::process::Command::new(&ffmpeg).args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"]).args(args).arg(out).status().unwrap();
        assert!(done.success(), "ffmpeg could not make {}", out.display());
        std::fs::read(out).unwrap()
    };
    let video = make(&["-f", "lavfi", "-i", "testsrc=duration=2:size=320x180:rate=30", "-pix_fmt", "yuv420p", "-an"], &tools.join("v.mp4"));
    let audio = make(&["-f", "lavfi", "-i", "sine=frequency=440:duration=2", "-vn"], &tools.join("a.m4a"));
    fx.server.route("/media/v.mp4", Route::bytes(video.clone(), "video/mp4"));
    fx.server.route("/media/a.m4a", Route::bytes(audio.clone(), "audio/mp4"));
    fx.server.route("/media/small.mp4", Route::pattern(50 * 1024, 150).content_type("video/mp4"));

    let piece = 16 * 1024u64;
    let page = serde_json::json!({
        "title": "A walk: in the hills?", "duration": 2.0, "is_live": false,
        "formats": [
            stream("299", "mp4", "avc1.64002A", "none", Some(1080), video.len() as u64, &fx.server.url("/media/v.mp4"), piece),
            // Picture and sound in one stream, as the smallest quality usually is.
            stream("18", "mp4", "avc1.42001E", "mp4a.40.2", Some(360), 50 * 1024, &fx.server.url("/media/small.mp4"), piece),
            stream("140", "m4a", "none", "mp4a.40.2", None, audio.len() as u64, &fx.server.url("/media/a.m4a"), piece),
        ],
    });
    std::fs::write(tools.join("page.json"), page.to_string()).unwrap();

    // What the dialog is told, before anything is downloaded.
    let address = "https://www.youtube.com/watch?v=testvideo01&list=PLxyz&index=3";
    let ticket = fx.manager.probe(Request::new(address));
    let deadline = Instant::now() + Duration::from_secs(20);
    let info = loop {
        if let Some(result) = ticket.take() {
            break result.unwrap();
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(info.variants.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(), vec!["1080", "360", "audio"]);
    assert_eq!(info.variant.as_deref(), Some("1080"));
    assert_eq!(info.total, Some((video.len() + audio.len()) as u64));
    assert_eq!(info.file_name, "A walk_ in the hills_.mp4", "the title, made safe for a file name");
    assert_eq!(fx.server.stats("/media/v.mp4").requests, 0, "describing the page fetches no media");

    let id = fx.manager.add(NewDownload::new(Request::new(address)));
    let done = fx.wait(id, "completion", completed);
    assert_eq!(done.url, "https://www.youtube.com/watch?v=testvideo01", "the same video whatever playlist the link came from");
    assert_eq!(done.file_name, "A walk_ in the hills_.mp4");
    assert_eq!(done.variant.as_deref(), Some("1080"));
    assert!(done.note.as_deref().unwrap_or("").starts_with("1080p"), "{:?}", done.note);
    assert_eq!(fx.manager.variants(id).len(), 3);

    // One file with both streams in it, and nothing else left behind.
    let probe = std::process::Command::new(ffmpeg.with_file_name("ffprobe")).args(["-v", "error", "-show_entries", "stream=codec_type", "-of", "csv=p=0"]).arg(done.path()).output().unwrap();
    let mut kinds: Vec<&str> = std::str::from_utf8(&probe.stdout).unwrap().lines().map(str::trim).collect();
    kinds.sort_unstable();
    assert_eq!(kinds, vec!["audio", "video"]);
    assert_eq!(files_in(&fx.downloads()), vec!["A walk_ in the hills_.mp4"]);

    // Every request stayed within the size the helper asked for, and carried its headers.
    let stats = fx.server.stats("/media/v.mp4");
    for range in stats.ranges.iter().filter(|r| !r.is_empty()) {
        let (from, to) = range.trim_start_matches("bytes=").split_once('-').unwrap();
        let asked = to.parse::<u64>().unwrap() - from.parse::<u64>().unwrap() + 1;
        assert!(asked <= piece, "a request asked for {asked} bytes ({range})");
    }
    assert!(stats.requests as usize > video.len() / piece as usize, "the stream was walked in pieces: {} requests", stats.requests);
    assert_eq!(stats.last_headers.get("user-agent").map(String::as_str), Some("Resolver/1.0"));
    assert!(std::fs::read_to_string(tools.join("calls.txt")).unwrap().lines().all(|call| call.contains("-J") && call.ends_with("watch?v=testvideo01")));

    // Sound only is one stream: no joining, and its own kind of file.
    let sound = fx.manager.add(NewDownload { request: Request::new("https://youtu.be/testvideo02"), variant: Some("audio".into()), ..Default::default() });
    let done = fx.wait(sound, "completion", completed);
    assert_eq!(done.file_name, "A walk_ in the hills_.m4a");
    assert_eq!(std::fs::read(done.path()).unwrap(), audio);

    // Another quality, chosen after the download was added, starts it over.
    let paused = fx.manager.add(NewDownload { request: Request::new("https://www.youtube.com/watch?v=testvideo03"), paused: true, ..Default::default() });
    fx.manager.set_variant(paused, "360");
    let done = fx.wait(paused, "completion at the other quality", completed);
    assert_eq!(done.variant.as_deref(), Some("360"));
    assert_eq!(done.total, Some(50 * 1024));
    fx.done();
}

#[cfg(unix)]
#[test]
fn a_video_page_carries_on_when_its_addresses_expire_and_says_why_when_it_cannot() {
    let fx = Fixture::new("page-expiry");
    let tools = fx.root.join("tools");
    fx.manager.set_resolver_for_tests(fake_resolver(&tools));
    let size = 6 * MIB;
    fx.server.route("/media/long.m4a", Route::pattern(size, 160).throttle(2 * MIB).token("a").drop_after(700 * 1024).content_type("audio/mp4"));
    let describe = |token: &str| {
        let page = serde_json::json!({ "title": "Long talk", "duration": 600, "formats": [stream("140", "m4a", "none", "mp4a.40.2", None, size, &fx.server.url(&format!("/media/long.m4a?token={token}")), MIB)] });
        std::fs::write(tools.join("page.json"), page.to_string()).unwrap();
    };
    describe("a");

    let id = fx.manager.add(NewDownload::new(Request::new("https://www.youtube.com/watch?v=longtalk001")));
    fx.wait(id, "some progress", |v| v.downloaded > MIB);
    // The addresses stop working; the page now gives out new ones.
    describe("b");
    fx.server.update("/media/long.m4a", |r| r.token = Some("b".into()));
    let done = fx.wait(id, "completion with the fresh addresses", completed);
    verify_file(&done.path(), 160, size).unwrap();
    assert!(fx.server.stats("/media/long.m4a").bytes_sent < size + 2 * MIB, "it carried on instead of starting over");
    assert!(std::fs::read_to_string(tools.join("calls.txt")).unwrap().lines().count() >= 2, "the page was read again");
    assert!(fx.manager.drain_events().iter().all(|e| !matches!(e, Event::Expired { .. })), "the user was not bothered");

    // The helper's own explanation reaches the user.
    std::fs::write(tools.join("error.txt"), "WARNING: something minor\nERROR: [youtube] gone0000001: Video unavailable. This video is private\n").unwrap();
    let gone = fx.manager.add(NewDownload::new(Request::new("https://www.youtube.com/watch?v=gone0000001")));
    let failed = fx.wait(gone, "the failure", |v| matches!(v.status, Status::Failed { .. }));
    assert!(matches!(&failed.status, Status::Failed { message, .. } if message.contains("Video unavailable. This video is private")), "{:?}", failed.status);

    // No helper at all.
    fx.manager.set_resolver_for_tests(tools.join("missing"));
    let lost = fx.manager.add(NewDownload::new(Request::new("https://www.youtube.com/watch?v=nohelper001")));
    let failed = fx.wait(lost, "the failure", |v| matches!(v.status, Status::Failed { .. }));
    assert!(matches!(&failed.status, Status::Failed { message, .. } if message.contains("could not be started")), "{:?}", failed.status);
    fx.done();
}

/// A file with picture and sound in one, as sites without separate streams
/// hand out: no codec names, no stated size.
fn whole(id: &str, width: u32, height: u32, url: &str) -> serde_json::Value {
    serde_json::json!({ "format_id": id, "ext": "mp4", "protocol": "https", "width": width, "height": height, "tbr": 800, "url": url })
}

#[cfg(unix)]
#[test]
fn a_quality_chosen_by_the_player_is_the_one_that_downloads() {
    let fx = Fixture::new("page-choice");
    let tools = fx.root.join("tools");
    fx.manager.set_resolver_for_tests(fake_resolver(&tools));
    fx.server.route("/media/720.mp4", Route::pattern(300 * 1024, 170).content_type("video/mp4"));
    fx.server.route("/media/360.mp4", Route::pattern(50 * 1024, 171).content_type("video/mp4"));
    let page = serde_json::json!({
        "title": "A phone that does it all https://t.example/abc", "uploader": "Val Phone", "extractor_key": "Facebook", "duration": 12,
        "formats": [whole("hd", 1280, 720, &fx.server.url("/media/720.mp4")), whole("sd", 640, 360, &fx.server.url("/media/360.mp4"))],
    });
    std::fs::write(tools.join("page.json"), page.to_string()).unwrap();

    // A post's address says nothing about a video being on it: the browser,
    // which saw the player, says so.
    let post = Request::new(fx.server.url("/someone/posts/3?ref=feed&utm_source=share")).with_referer("https://site.example/feed").as_page();
    assert_eq!(post.protocol(), Protocol::Page);
    assert_eq!(Request::new(post.url.clone()).protocol(), Protocol::Http);

    // What the menu by the player is told.
    let info = fx.server_runtime.block_on(fx.manager.describe_page(&post)).unwrap();
    assert_eq!(info.name, "Val Phone - A phone that does it all", "who posted it and what they wrote, without the link");
    assert_eq!(info.variants.iter().map(|v| (v.id.as_str(), v.label.as_str(), v.container.as_str())).collect::<Vec<_>>(), vec![("720", "720p", "mp4"), ("360", "360p", "mp4")]);
    assert_eq!(info.default.as_deref(), Some("720"));
    assert_eq!(info.duration, Some(12));
    assert!(info.variants.iter().all(|v| v.size_estimated && v.size == Some(1_200_000)), "800 kbit/s for 12 s, said to be an estimate: {:?}", info.variants);

    // The user picks the smaller one there; that is what starts, at once.
    fx.manager.capture(Capture { request: post.clone(), page_title: Some("Feed".into()), media: true, variant: Some("360".into()), ..Default::default() });
    let mut seen = Vec::new();
    let id = match fx.event(&mut seen, "the capture", |e| matches!(e, Event::Captured { .. })) {
        Event::Captured { provisional, capture } => {
            assert_eq!(capture.variant.as_deref(), Some("360"));
            provisional.expect("a video page starts ahead like anything else")
        }
        _ => unreachable!(),
    };
    let done = fx.wait(id, "completion", completed);
    assert_eq!(done.protocol, Protocol::Page);
    assert_eq!(done.variant.as_deref(), Some("360"));
    assert_eq!(done.total, Some(50 * 1024), "the real size, asked of the server, replaces the estimate");
    assert_eq!(done.file_name, "Val Phone - A phone that does it all.mp4");
    assert!(done.url.ends_with("/someone/posts/3"), "how the visitor got there is not part of the address: {}", done.url);
    verify_file(&done.path(), 171, 50 * 1024).unwrap();
    assert_eq!(fx.server.stats("/media/720.mp4").requests, 0, "the quality that was not chosen is never touched");
    assert_eq!(std::fs::read_to_string(tools.join("calls.txt")).unwrap().lines().count(), 1, "the menu and the download read the page once between them");

    // The dialog offers the same choices, and the list knows the address
    // however it is written.
    assert_eq!(fx.manager.variants(id).len(), 2);
    fx.manager.confirm(id, Confirmation::default()).unwrap();
    assert_eq!(fx.manager.duplicate_of(&fx.server.url("/someone/posts/3?utm_source=other")).map(|d| d.id), Some(id));
    fx.done();
}

#[cfg(unix)]
#[test]
fn a_page_that_wants_a_login_is_read_again_with_the_browsers_cookies() {
    let fx = Fixture::new("page-login");
    let tools = fx.root.join("tools");
    fx.manager.set_resolver_for_tests(fake_resolver(&tools));
    fx.server.route("/media/clip.mp4", Route::pattern(40 * 1024, 172).content_type("video/mp4"));
    let page = serde_json::json!({ "title": "Wait for it", "uploader": "r/aww", "extractor_key": "Reddit", "duration": 8, "formats": [whole("fallback", 640, 360, &fx.server.url("/media/clip.mp4"))] });
    std::fs::write(tools.join("page.json"), page.to_string()).unwrap();
    std::fs::write(tools.join("login.txt"), "").unwrap();
    let calls = || std::fs::read_to_string(tools.join("calls.txt")).unwrap_or_default();
    let describe = |request: &Request| fx.server_runtime.block_on(fx.manager.describe_page(request));

    // Nothing to log in with: the site's refusal is passed on, without the
    // helper's advice about its own command line.
    let anonymous = Request::new(fx.server.url("/r/aww/comments/1/wait")).as_page();
    let refused = describe(&anonymous).unwrap_err().to_string();
    assert!(refused.contains("Account authentication is required") && !refused.contains("--cookies") && !refused.contains("github.com"), "{refused}");
    assert_eq!(calls().lines().count(), 1);

    // The browser handed its cookies over: logged out first, then with them.
    let mut member = Request::new(fx.server.url("/r/aww/comments/2/wait")).as_page();
    member.set_header("Cookie", "reddit_session=abc123; theme=dark");
    let info = describe(&member).unwrap();
    assert_eq!(info.name, "r_aww - Wait for it".replace('_', "/"));
    let log = calls();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 3);
    assert!(!lines[1].contains("--cookies"), "logged out first: {}", lines[1]);
    assert!(lines[2].contains("--cookies "), "then with the login: {}", lines[2]);
    let jar = std::fs::read_to_string(tools.join("seen-cookies.txt")).unwrap();
    assert!(jar.starts_with("# Netscape HTTP Cookie File\n") && jar.contains("\treddit_session\tabc123\n") && jar.contains("\ttheme\tdark\n"), "{jar}");
    let jar_path = lines[2].split("--cookies ").nth(1).unwrap().split(" -J").next().unwrap();
    assert!(!Path::new(jar_path).exists(), "the cookie file does not outlive the reading: {jar_path}");

    // The download carries the login the same way.
    let id = fx.manager.add(NewDownload::new(member.clone()));
    let done = fx.wait(id, "completion", completed);
    verify_file(&done.path(), 172, 40 * 1024).unwrap();

    // A YouTube account is never used this way.
    let mut tube = Request::new("https://www.youtube.com/watch?v=membersonly1");
    tube.set_header("Cookie", "SID=secret");
    assert!(describe(&tube).is_err());
    assert!(calls().lines().filter(|line| line.contains("membersonly1")).all(|line| !line.contains("--cookies")), "{}", calls());

    // And nobody's is, once the user has said no.
    let mut settings = fx.manager.settings();
    settings.page_login = false;
    fx.manager.set_settings(settings).unwrap();
    let mut other = Request::new(fx.server.url("/r/aww/comments/3/wait")).as_page();
    other.set_header("Cookie", "reddit_session=abc123");
    assert!(describe(&other).is_err());
    assert!(calls().lines().filter(|line| line.contains("/comments/3/")).all(|line| !line.contains("--cookies")));
    fx.done();
}

#[test]
fn a_stream_is_fetched_in_the_quality_that_was_chosen() {
    let fx = Fixture::new("stream-choice");
    serve_stream(&fx.server, "show/720", 4, 120 * 1024, 8 * MIB);
    serve_stream(&fx.server, "show/360", 4, 40 * 1024, 8 * MIB);
    let master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1280x720\n720/index.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\n360/index.m3u8\n";
    fx.server.route("/show/master.m3u8", Route::bytes(master, "application/vnd.apple.mpegurl"));
    let address = fx.server.url("/show/master.m3u8");

    // The dialog learns what the stream comes in.
    let ticket = fx.manager.probe(Request::new(address.clone()));
    let deadline = Instant::now() + Duration::from_secs(20);
    let info = loop {
        if let Some(result) = ticket.take() {
            break result.unwrap();
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(info.variants.iter().map(|v| v.label.as_str()).collect::<Vec<_>>(), vec!["720p", "360p"]);
    assert_eq!(info.variant.as_deref(), Some("720"));

    // Chosen by the player: only that rendition is fetched.
    let id = fx.manager.add(NewDownload { request: Request::new(address.clone()), file_name: Some("Show.mp4".into()), variant: Some("360".into()), ..Default::default() });
    let done = fx.wait(id, "completion", completed);
    assert!(done.note.as_deref().unwrap_or("").starts_with("360p"), "{:?}", done.note);
    assert_eq!(fx.server.stats("/show/360/seg3.ts").requests, 1);
    assert_eq!(fx.server.stats("/show/720/seg0.ts").requests, 0, "the other quality is never touched");
    assert_eq!(fx.manager.variants(id).len(), 2);

    // A quality between two renditions takes the one below; none takes the best.
    let between = fx.manager.add(NewDownload { request: Request::new(format!("{address}?again=1")), variant: Some("480".into()), ..Default::default() });
    assert!(fx.wait(between, "completion", completed).note.as_deref().unwrap_or("").starts_with("360p"));
    let best = fx.manager.add(NewDownload::new(Request::new(format!("{address}?again=2"))));
    assert!(fx.wait(best, "completion", completed).note.as_deref().unwrap_or("").starts_with("720p"));
    assert_eq!(fx.server.stats("/show/720/seg3.ts").requests, 1);
    fx.done();
}

#[cfg(unix)]
#[test]
fn a_page_on_a_site_that_only_streams_goes_through_the_stream_engine() {
    let fx = Fixture::new("page-stream");
    let tools = fx.root.join("tools");
    fx.manager.set_resolver_for_tests(fake_resolver(&tools));
    serve_stream(&fx.server, "talk/1080", 3, 90 * 1024, 8 * MIB);
    serve_stream(&fx.server, "talk/480", 3, 30 * 1024, 8 * MIB);
    let master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=4000000,RESOLUTION=1920x1080\n1080/index.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=854x480\n480/index.m3u8\n";
    fx.server.route("/talk/master.m3u8", Route::bytes(master, "application/vnd.apple.mpegurl"));
    let rendition = |id: &str, width: u32, height: u32| {
        serde_json::json!({ "format_id": id, "ext": "mp4", "protocol": "m3u8_native", "vcodec": "avc1.640028", "acodec": "mp4a.40.2", "width": width, "height": height, "tbr": 1000,
            "url": fx.server.url(&format!("/talk/{height}/index.m3u8")), "manifest_url": fx.server.url("/talk/master.m3u8") })
    };
    let page = serde_json::json!({ "title": "A talk", "uploader": "Someone", "extractor_key": "Vimeo", "duration": 18, "formats": [rendition("hls-1", 1920, 1080), rendition("hls-2", 854, 480)] });
    std::fs::write(tools.join("page.json"), page.to_string()).unwrap();

    let request = Request::new("https://vimeo.com/76979871?share=copy&ref=feed");
    assert_eq!(request.protocol(), Protocol::Page, "known by its shape, typed or pasted");
    let info = fx.server_runtime.block_on(fx.manager.describe_page(&request)).unwrap();
    assert_eq!(info.variants.iter().map(|v| (v.id.as_str(), v.detail.as_str())).collect::<Vec<_>>(), vec![("1080", "MP4  \u{00B7}  H.264  \u{00B7}  stream"), ("480", "MP4  \u{00B7}  H.264  \u{00B7}  stream")]);
    assert_eq!(info.name, "A talk", "a titled video keeps its title");

    let id = fx.manager.add(NewDownload { request, variant: Some("480".into()), ..Default::default() });
    let done = fx.wait(id, "completion", completed);
    assert!(done.file_name.starts_with("A talk."), "{}", done.file_name);
    assert_eq!(done.variant.as_deref(), Some("480"));
    assert_eq!(fx.server.stats("/talk/480/seg2.ts").requests, 1);
    assert_eq!(fx.server.stats("/talk/1080/seg0.ts").requests, 0);
    fx.done();
}

/// Asks the real helper about a real page (an openly licensed film), so it
/// needs the network and an installed `yt-dlp`, and runs only on request:
/// `cargo test -p siphon-core --test workflow real_video_page -- --ignored --nocapture`.
/// Only the description is fetched; no media.
#[test]
#[ignore = "needs the network and yt-dlp"]
fn a_real_video_page_is_described() {
    let fx = Fixture::new("page-real");
    let ticket = fx.manager.probe(Request::new("https://www.youtube.com/watch?v=aqz-KE-bpKQ&list=PLtest&t=42s"));
    let deadline = Instant::now() + Duration::from_secs(90);
    let info = loop {
        if let Some(result) = ticket.take() {
            break result.expect("the page is described");
        }
        assert!(Instant::now() < deadline, "the helper did not answer");
        std::thread::sleep(Duration::from_millis(50));
    };
    println!("file: {}\nfetched first: {:?} ({:?} bytes)\nnote: {:?}", info.file_name, info.variant, info.total, info.note);
    for variant in &info.variants {
        println!("  {:>10}  {:<16} {:>12}  .{}", variant.label, variant.detail, variant.size.map(|s| format!("{:.1} MB", s as f64 / 1048576.0)).unwrap_or_default(), variant.container);
    }
    assert!(info.file_name.ends_with(".mp4") || info.file_name.ends_with(".webm") || info.file_name.ends_with(".mkv"));
    assert_eq!(info.variant.as_deref(), Some("1080"));
    assert!(info.variants.len() >= 4 && info.variants.iter().any(|v| v.id == "audio"));
    assert!(info.total.is_some_and(|t| t > 10 * MIB));
    assert_eq!(info.final_url, "https://www.youtube.com/watch?v=aqz-KE-bpKQ");
    fx.done();
}
