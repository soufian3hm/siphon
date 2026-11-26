//! The manager as the application uses it: queueing, persistence, link
//! refresh, browser captures, restarts.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use siphon_core::filename::{meta_path, part_path};
use siphon_core::net::ProxyMode;
use siphon_core::secrets::Secrets;
use siphon_core::segments::StealPolicy;
use siphon_core::transfer::Tuning;
use siphon_core::types::{DownloadId, Request, Status};
use siphon_core::{Capture, DownloadView, Error, Event, Manager, NewDownload, Settings};
use siphon_testserver::{Route, TestServer, verify_file};

const MIB: u64 = 1024 * 1024;

struct Fixture {
    root: PathBuf,
    manager: Manager,
    server: TestServer,
    /// The server outlives any one manager, as a real server outlives the app.
    server_runtime: tokio::runtime::Runtime,
}

impl Fixture {
    fn new(name: &str) -> Self {
        Self::with(name, |_| {})
    }

    fn with(name: &str, configure: impl FnOnce(&mut Settings)) -> Self {
        let root = std::env::temp_dir().join(format!("siphon-mgr-{}-{name}", std::process::id()));
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

    fn add(&self, path: &str) -> DownloadId {
        self.manager.add(NewDownload::new(Request::new(self.server.url(path))))
    }

    fn wait(&self, id: DownloadId, what: &str, test: impl Fn(&DownloadView) -> bool) -> DownloadView {
        wait(&self.manager, id, what, test)
    }

    fn reopen(mut self, configure: impl FnOnce(&mut Settings)) -> Self {
        self.manager.shutdown();
        let manager = open(&self.root, configure);
        Self { root: self.root.clone(), manager, server: self.server, server_runtime: self.server_runtime }
    }
}

fn open(root: &Path, configure: impl FnOnce(&mut Settings)) -> Manager {
    let manager = Manager::open_with(&root.join("data"), Secrets::in_memory(), || {}).unwrap();
    let mut settings = manager.settings();
    settings.download_dir = root.join("downloads");
    settings.net.proxy.mode = ProxyMode::Direct;
    settings.connections = 4;
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

fn wait(manager: &Manager, id: DownloadId, what: &str, test: impl Fn(&DownloadView) -> bool) -> DownloadView {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let view = manager.get(id).unwrap_or_else(|| panic!("download {id} vanished while waiting for: {what}"));
        if test(&view) {
            return view;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}; last state {:?}", view.status);
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn completed(view: &DownloadView) -> bool {
    view.status == Status::Completed
}

#[test]
fn downloads_names_the_file_and_survives_a_restart() {
    let fx = Fixture::new("basic");
    let size = 3 * MIB + 17;
    fx.server.route("/get", Route::pattern(size, 1).disposition("attachment; filename=\"Quarterly Report.pdf\"").content_type("application/pdf"));

    let id = fx.add("/get");
    let view = fx.wait(id, "completion", completed);
    assert_eq!(view.file_name, "Quarterly Report.pdf");
    assert_eq!(view.total, Some(size));
    assert_eq!(view.downloaded, size);
    assert_eq!(view.fraction, Some(1.0));
    assert_eq!(view.resumable, Some(true));
    verify_file(&fx.downloads().join("Quarterly Report.pdf"), 1, size).unwrap();
    assert!(fx.manager.drain_events().iter().any(|e| matches!(e, Event::Completed { id: done, .. } if *done == id)));

    let fx = fx.reopen(|_| {});
    let list = fx.manager.snapshot();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].status, Status::Completed);
    assert_eq!(list[0].file_name, "Quarterly Report.pdf");
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn the_queue_respects_the_concurrency_limit() {
    let fx = Fixture::with("queue", |s| s.max_active = 2);
    for i in 0..5 {
        fx.server.route(&format!("/f{i}.bin"), Route::pattern(MIB, 10 + i).throttle(2 * MIB));
    }
    let ids: Vec<DownloadId> = (0..5).map(|i| fx.add(&format!("/f{i}.bin"))).collect();

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut peak = 0;
    loop {
        let list = fx.manager.snapshot();
        let active = list.iter().filter(|v| v.status.is_active()).count();
        peak = peak.max(active);
        assert!(active <= 2, "{active} downloads active with a limit of 2");
        if list.iter().all(completed) {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(peak, 2);
    for (i, id) in ids.iter().enumerate() {
        let view = fx.manager.get(*id).unwrap();
        verify_file(&view.path(), 10 + i as u64, MIB).unwrap();
    }
    // Newest first in the list.
    assert_eq!(fx.manager.snapshot().first().unwrap().id, *ids.last().unwrap());
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn pause_resume_and_live_numbers() {
    let fx = Fixture::new("pause");
    let size = 8 * MIB;
    fx.server.route("/slow.bin", Route::pattern(size, 2).throttle(2 * MIB));
    let id = fx.add("/slow.bin");

    let running = fx.wait(id, "some progress", |v| v.downloaded > MIB && v.bytes_per_sec > 0.0);
    assert_eq!(running.status, Status::Downloading);
    assert!(running.connections >= 2);
    assert!(running.eta_secs.is_some());
    let details = fx.manager.details(id).unwrap();
    assert!(details.segments.len() >= 4, "{:?}", details.segments);
    assert!(details.segments.iter().any(|s| s.active));

    fx.manager.pause(id);
    let paused = fx.wait(id, "pause", |v| v.status == Status::Paused);
    assert!(paused.downloaded > MIB && paused.downloaded < size);
    assert_eq!(paused.bytes_per_sec, 0.0);
    assert_eq!(paused.connections, 0);
    // The byte map of a stopped download is read from its sidecar.
    assert!(!fx.manager.details(id).unwrap().segments.is_empty());

    fx.manager.resume(id);
    fx.wait(id, "completion", completed);
    verify_file(&fx.downloads().join("slow.bin"), 2, size).unwrap();
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn quitting_mid_download_continues_on_the_next_launch() {
    let fx = Fixture::new("relaunch");
    let size = 8 * MIB;
    fx.server.route("/big.bin", Route::pattern(size, 3).throttle(2 * MIB));
    let id = fx.add("/big.bin");
    let before = fx.wait(id, "some progress", |v| v.downloaded > 2 * MIB).downloaded;

    let sent_before = fx.server.stats("/big.bin").bytes_sent;
    let fx = fx.reopen(|_| {});
    let view = fx.manager.get(id).unwrap();
    assert!(view.downloaded >= before / 2, "progress was lost across the restart: {}", view.downloaded);
    fx.wait(id, "completion after relaunch", completed);
    verify_file(&fx.downloads().join("big.bin"), 3, size).unwrap();
    let resent = fx.server.stats("/big.bin").bytes_sent - sent_before;
    assert!(resent < size - before / 2, "the relaunch started over ({resent} bytes re-sent)");
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn with_resume_on_launch_off_downloads_wait_paused() {
    let fx = Fixture::new("noresume");
    fx.server.route("/big.bin", Route::pattern(8 * MIB, 4).throttle(2 * MIB));
    let id = fx.add("/big.bin");
    fx.wait(id, "some progress", |v| v.downloaded > MIB);

    // The setting is read when the library is opened, so store it first.
    let mut settings = fx.manager.settings();
    settings.resume_on_launch = false;
    fx.manager.set_settings(settings).unwrap();
    let fx = fx.reopen(|s| s.resume_on_launch = false);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(fx.manager.get(id).unwrap().status, Status::Paused);
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn an_expired_link_is_swapped_and_the_download_continues() {
    let fx = Fixture::new("refresh");
    let size = 8 * MIB;
    // An expired link is only noticed when a new request is made. The server
    // cuts every response short so that connections have to come back.
    fx.server.route("/signed.bin", Route::pattern(size, 5).throttle(2 * MIB).token("one").drop_after(700 * 1024));
    fx.server.route("/other.bin", Route::pattern(size + 1, 6));
    fx.server.route("/dead.bin", Route::pattern(size, 7).token("never"));

    let request = Request::new(fx.server.url("/signed.bin?token=one")).with_referer("https://files.example/page");
    let id = fx.manager.add(NewDownload::new(request));
    fx.wait(id, "some progress", |v| v.downloaded > MIB);
    fx.server.update("/signed.bin", |r| r.token = Some("two".into()));

    let stalled = fx.wait(id, "expiry", |v| matches!(v.status, Status::NeedsRefresh { .. }));
    assert_eq!(stalled.status, Status::NeedsRefresh { code: 403 });
    assert!(stalled.downloaded > MIB, "the partial data must be kept");
    assert!(part_path(&stalled.path()).exists() && meta_path(&stalled.path()).exists());
    assert!(fx.manager.drain_events().iter().any(|e| matches!(e, Event::Expired { .. })));

    let finish = |ticket: siphon_core::Ticket<Result<(), Error>>| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(result) = ticket.take() {
                return result;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    };

    // A link to a different file is refused and nothing changes.
    let wrong = finish(fx.manager.refresh_link(id, Request::new(fx.server.url("/other.bin"))));
    assert!(matches!(wrong, Err(Error::Mismatch(_))), "{wrong:?}");
    // So is a link that is itself dead.
    let dead = finish(fx.manager.refresh_link(id, Request::new(fx.server.url("/dead.bin?token=x"))));
    assert!(matches!(dead, Err(Error::Mismatch(_))), "{dead:?}");
    let still = fx.manager.get(id).unwrap();
    assert_eq!(still.status, Status::NeedsRefresh { code: 403 });
    assert!(still.url.ends_with("token=one"));

    let kept = still.downloaded;
    let sent_before = fx.server.stats("/signed.bin").bytes_sent;
    finish(fx.manager.refresh_link(id, Request::new(fx.server.url("/signed.bin?token=two")))).unwrap();
    let done = fx.wait(id, "completion on the fresh link", completed);
    assert!(done.url.ends_with("token=two"));
    assert_eq!(done.referer.as_deref(), Some("https://files.example/page"), "the source page is remembered");
    verify_file(&done.path(), 5, size).unwrap();
    let resent = fx.server.stats("/signed.bin").bytes_sent - sent_before;
    assert!(resent < size - kept + MIB, "the fresh link started over");
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn captures_ask_first_or_start_at_once_depending_on_the_setting() {
    let fx = Fixture::new("capture");
    fx.server.route("/cap.zip", Route::pattern(MIB, 8));
    let capture = Capture {
        request: Request::new(fx.server.url("/cap.zip")).with_referer("https://site.example/"),
        suggested_name: Some("cap.zip".into()),
        size: Some(MIB),
        ..Default::default()
    };

    // Default: confirm first.
    fx.manager.capture(capture.clone());
    let events = fx.manager.drain_events();
    let provisional = match events.as_slice() {
        [Event::Captured { capture: c, provisional }] if c == &capture => *provisional,
        other => panic!("{other:?}"),
    };
    assert!(fx.manager.snapshot().is_empty(), "nothing joins the list before the user confirms");
    // It was fetched in the background meanwhile; saying no deletes it.
    fx.manager.discard(provisional.expect("instant start is on by default"));

    // Save automatically: straight into the default folder.
    let mut settings = fx.manager.settings();
    settings.save_automatically = true;
    fx.manager.set_settings(settings).unwrap();
    fx.manager.capture(capture);
    let deadline = Instant::now() + Duration::from_secs(10);
    let id = loop {
        if let Some(Event::AutoStarted { id, .. }) = fx.manager.drain_events().into_iter().find(|e| matches!(e, Event::AutoStarted { .. })) {
            break id;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    };
    let done = fx.wait(id, "completion", completed);
    assert_eq!(done.dir, fx.downloads());
    verify_file(&done.path(), 8, MIB).unwrap();
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn a_captured_link_revives_the_stalled_download_it_belongs_to() {
    let fx = Fixture::with("revive", |s| s.save_automatically = true);
    let size = 6 * MIB;
    // An expired link is only noticed when a new request is made. The server
    // cuts every response short so that connections have to come back.
    fx.server.route("/movie.mkv", Route::pattern(size, 9).throttle(2 * MIB).token("a").drop_after(700 * 1024));
    let id = fx.manager.add(NewDownload::new(Request::new(fx.server.url("/movie.mkv?token=a"))));
    fx.wait(id, "some progress", |v| v.downloaded > MIB);
    fx.server.update("/movie.mkv", |r| r.token = Some("b".into()));
    fx.wait(id, "expiry", |v| matches!(v.status, Status::NeedsRefresh { .. }));
    assert_eq!(fx.manager.refresh_candidate(Some(size), Some("movie.mkv")), Some(id));
    assert_eq!(fx.manager.refresh_candidate(Some(size + 1), Some("movie.mkv")), None);
    fx.manager.drain_events();

    // The user goes back to the page and clicks download again; the browser
    // hands over a brand new URL for the same file.
    fx.manager.capture(Capture {
        request: Request::new(fx.server.url("/movie.mkv?token=b")),
        suggested_name: Some("movie.mkv".into()),
        size: Some(size),
        ..Default::default()
    });
    let done = fx.wait(id, "completion", completed);
    verify_file(&done.path(), 9, size).unwrap();
    assert_eq!(fx.manager.snapshot().len(), 1, "no duplicate download was created");
    assert!(fx.manager.drain_events().iter().any(|e| matches!(e, Event::AutoRefreshed { id: revived, .. } if *revived == id)));
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn removing_cleans_up_partials_but_keeps_finished_files() {
    let fx = Fixture::new("remove");
    fx.server.route("/keep.bin", Route::pattern(MIB, 20));
    fx.server.route("/drop.bin", Route::pattern(8 * MIB, 21).throttle(2 * MIB));

    let keep = fx.add("/keep.bin");
    fx.wait(keep, "completion", completed);
    let drop_id = fx.add("/drop.bin");
    fx.wait(drop_id, "some progress", |v| v.downloaded > MIB);
    let partial = part_path(&fx.downloads().join("drop.bin"));
    assert!(partial.exists());

    // Removing a running download stops it first.
    fx.manager.remove(drop_id, false);
    // The entry leaves the list first and its files are deleted right after,
    // so wait for all three rather than checking in between.
    let sidecar = meta_path(&fx.downloads().join("drop.bin"));
    let deadline = Instant::now() + Duration::from_secs(10);
    while fx.manager.get(drop_id).is_some() || partial.exists() || sidecar.exists() {
        assert!(Instant::now() < deadline, "the running download was not removed");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!fx.downloads().join("drop.bin").exists(), "an unfinished download leaves no file behind");

    fx.manager.remove(keep, false);
    assert!(fx.manager.get(keep).is_none());
    assert!(fx.downloads().join("keep.bin").exists(), "removing from the list must not delete the file");
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn names_never_collide_and_user_names_are_kept() {
    let fx = Fixture::new("names");
    fx.server.route("/same.bin", Route::pattern(MIB, 22).throttle(4 * MIB));
    let a = fx.add("/same.bin");
    let b = fx.add("/same.bin");
    let c = fx.manager.add(NewDownload {
        request: Request::new(fx.server.url("/same.bin")),
        file_name: Some("My Choice.dat".into()),
        ..Default::default()
    });
    for id in [a, b, c] {
        fx.wait(id, "completion", completed);
    }
    let mut names: Vec<String> = [a, b, c].iter().map(|id| fx.manager.get(*id).unwrap().file_name).collect();
    names.sort();
    assert_eq!(names, ["My Choice.dat", "same (1).bin", "same.bin"]);
    for name in &names {
        verify_file(&fx.downloads().join(name), 22, MIB).unwrap();
    }
    assert_eq!(fx.manager.find_by_url(&fx.server.url("/same.bin")), None, "finished downloads are not duplicates");
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn restart_move_and_rename() {
    let fx = Fixture::new("ops");
    let size = 6 * MIB;
    fx.server.route("/ops.bin", Route::pattern(size, 23).throttle(2 * MIB));
    let id = fx.add("/ops.bin");
    fx.wait(id, "some progress", |v| v.downloaded > MIB);

    // A running download refuses to be moved or renamed.
    let elsewhere = fx.root.join("elsewhere");
    assert!(fx.manager.move_to(id, &elsewhere).is_err());
    assert!(fx.manager.rename(id, "new.bin").is_err());

    fx.manager.pause(id);
    let paused = fx.wait(id, "pause", |v| v.status == Status::Paused);
    let kept = paused.downloaded;

    // Moving a paused download takes its partial data along.
    fx.manager.move_to(id, &elsewhere).unwrap();
    fx.manager.rename(id, "renamed.bin").unwrap();
    assert!(part_path(&elsewhere.join("renamed.bin")).exists());
    assert!(meta_path(&elsewhere.join("renamed.bin")).exists());
    assert!(!part_path(&fx.downloads().join("ops.bin")).exists());

    let sent_before = fx.server.stats("/ops.bin").bytes_sent;
    fx.manager.resume(id);
    let done = fx.wait(id, "completion after move", completed);
    assert_eq!(done.path(), elsewhere.join("renamed.bin"));
    verify_file(&done.path(), 23, size).unwrap();
    assert!(fx.server.stats("/ops.bin").bytes_sent - sent_before < size - kept + MIB, "moving lost the progress");

    // A finished file can be moved too, and renaming onto an existing name fails.
    fx.manager.move_to(id, &fx.downloads()).unwrap();
    assert!(fx.downloads().join("renamed.bin").exists());
    std::fs::write(fx.downloads().join("taken.bin"), b"x").unwrap();
    assert!(fx.manager.rename(id, "taken.bin").is_err());

    // Restart throws the file state away and fetches everything again.
    fx.manager.restart(id);
    fx.wait(id, "restart", |v| v.status.is_active() || v.status == Status::Queued);
    let again = fx.wait(id, "completion after restart", completed);
    verify_file(&again.path(), 23, size).unwrap();
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn failures_retry_by_themselves_and_bad_links_say_why() {
    let fx = Fixture::new("failures");
    fx.server.route("/flaky.bin", Route::pattern(MIB, 24).fail_first(2));
    fx.server.route("/gone.bin", Route::pattern(MIB, 25).token("x").denied_status(410));

    // A server that refuses the first requests: the probe's own retries get through.
    let flaky = fx.add("/flaky.bin");
    fx.wait(flaky, "completion", completed);

    let gone = fx.add("/gone.bin");
    let view = fx.wait(gone, "expiry", |v| matches!(v.status, Status::NeedsRefresh { .. }));
    assert_eq!(view.status, Status::NeedsRefresh { code: 410 });

    let nowhere = fx.manager.add(NewDownload::new(Request::new("http://127.0.0.1:9/nothing.bin")));
    let view = fx.wait(nowhere, "failure", |v| matches!(v.status, Status::Failed { .. }));
    match &view.status {
        Status::Failed { message, retryable } => {
            assert!(*retryable);
            assert!(message.to_lowercase().contains("refused"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert!(view.retry_in.is_some(), "a network failure schedules its own retry");

    // Pausing cancels the scheduled retry.
    fx.manager.pause(nowhere);
    assert!(fx.manager.get(nowhere).unwrap().retry_in.is_none());

    let bad = fx.manager.add(NewDownload::new(Request::new("notaurl")));
    let view = fx.wait(bad, "failure", |v| matches!(v.status, Status::Failed { .. }));
    assert!(matches!(view.status, Status::Failed { retryable: false, .. }), "{:?}", view.status);
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn probing_reports_what_the_dialog_needs() {
    let fx = Fixture::new("probe");
    fx.server.route("/v.mp4", Route::pattern(5 * MIB, 26).content_type("video/mp4"));
    fx.server.route("/s", Route::pattern(100, 27).no_length().content_type("audio/mpeg"));
    fx.server.route("/x", Route::pattern(100, 28).token("t"));

    let take = |url: String| {
        let ticket = fx.manager.probe(Request::new(url));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(result) = ticket.take() {
                return result;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let info = take(fx.server.url("/v.mp4")).unwrap();
    assert_eq!((info.file_name.as_str(), info.total, info.resumable), ("v.mp4", Some(5 * MIB), true));
    let info = take(fx.server.url("/s")).unwrap();
    assert_eq!((info.file_name.as_str(), info.total, info.resumable), ("s.mp3", None, false));
    assert!(matches!(take(fx.server.url("/x")), Err(Error::Expired(403))));
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn the_speed_limit_applies_to_running_downloads_immediately() {
    let fx = Fixture::new("limit");
    let size = 6 * MIB;
    fx.server.route("/fast.bin", Route::pattern(size, 29));
    let mut settings = fx.manager.settings();
    settings.speed_limit = MIB;
    settings.speed_limit_enabled = true;
    fx.manager.set_settings(settings.clone()).unwrap();

    let started = Instant::now();
    let id = fx.add("/fast.bin");
    fx.wait(id, "some progress", |v| v.downloaded > MIB);
    // Unlimited, this megabyte takes a few milliseconds. The limiter is exact
    // over time but lets a short burst and the chunk already in hand through
    // first, so the floor is well under the nominal second.
    assert!(started.elapsed() >= Duration::from_millis(300), "the limit was ignored");

    settings.speed_limit_enabled = false;
    fx.manager.set_settings(settings).unwrap();
    let lifted = Instant::now();
    fx.wait(id, "completion", completed);
    assert!(lifted.elapsed() < Duration::from_secs(3), "lifting the limit had no effect");
    std::fs::remove_dir_all(&fx.root).unwrap();
}
