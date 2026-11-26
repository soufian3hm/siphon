//! The engine against a real socket and a server that misbehaves on purpose.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use siphon_core::error::Error;
use siphon_core::filename::{meta_path, part_path};
use siphon_core::http::HttpSource;
use siphon_core::net::{NetConfig, ProxyMode, build_client};
use siphon_core::rate::Limiter;
use siphon_core::segments::StealPolicy;
use siphon_core::source::Source;
use siphon_core::transfer::{Outcome, Plan, Progress, Tuning, run};
use siphon_core::types::Request;
use siphon_testserver::{Route, TestServer, verify_file};
use tokio_util::sync::CancellationToken;

const MIB: u64 = 1024 * 1024;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("siphon-it-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn client() -> reqwest::Client {
    let mut config = NetConfig::default();
    config.proxy.mode = ProxyMode::Direct;
    config.stall_timeout_secs = 5;
    build_client(&config, None).unwrap()
}

fn tuning(connections: usize) -> Tuning {
    Tuning {
        connections,
        steal: StealPolicy { min_split: 64 * 1024, min_time: Duration::ZERO },
        checkpoint_every: Duration::from_millis(80),
        retry_limit: 5,
        backoff_base: Duration::from_millis(15),
        backoff_cap: Duration::from_millis(150),
        flush_bytes: 64 * 1024,
        flush_every: Duration::from_millis(40),
        max_request: None,
    }
}

struct Job {
    progress: Arc<Progress>,
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<Outcome>,
}

/// Probes and starts a run in the background, the way the manager does.
async fn start(url: &str, dest: &Path, tuning: Tuning, limiter: Arc<Limiter>) -> Result<Job, Error> {
    let source = Arc::new(Source::Http(HttpSource::new(client(), Request::new(url))?));
    let (probe, body) = source.probe().await?;
    let plan = Plan { source, final_path: dest.to_owned(), probe, body, tuning };
    let progress = Arc::new(Progress::default());
    let cancel = CancellationToken::new();
    let handle = tokio::spawn(run(plan, progress.clone(), limiter, cancel.clone()));
    Ok(Job { progress, cancel, handle })
}

async fn download(url: &str, dest: &Path, tuning: Tuning) -> Outcome {
    match start(url, dest, tuning, Arc::new(Limiter::default())).await {
        Ok(job) => job.handle.await.unwrap(),
        Err(e) => Outcome::Failed(e),
    }
}

async fn wait_for(progress: &Progress, at_least: u64) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while progress.downloaded() < at_least {
        assert!(Instant::now() < deadline, "stuck at {} bytes", progress.downloaded());
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn assert_completed(outcome: Outcome, dest: &Path, seed: u64, size: u64) {
    match outcome {
        Outcome::Completed { path, bytes } => {
            assert_eq!(path, dest);
            assert_eq!(bytes, size);
        }
        other => panic!("expected completion, got {other:?}"),
    }
    verify_file(dest, seed, size).unwrap();
    assert!(!part_path(dest).exists(), "the partial file must be renamed away");
    assert!(!meta_path(dest).exists(), "the sidecar must be removed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn segmented_download_is_byte_exact() {
    let server = TestServer::start().await;
    let size = 24 * MIB + 12_345;
    server.route("/big.bin", Route::pattern(size, 1).etag("\"v1\""));
    let dir = scratch("segmented");
    let dest = dir.join("big.bin");

    let outcome = download(&server.url("/big.bin"), &dest, tuning(8)).await;
    assert_completed(outcome, &dest, 1, size);

    let stats = server.stats("/big.bin");
    assert!(stats.peak_connections >= 4, "expected parallel connections, peak was {}", stats.peak_connections);

    // Ranges are handed out before any request is sent, so the eight opening
    // requests tile the file exactly: no gap, no overlap, no request for the
    // whole file that then has to be cut short.
    let mut opening: Vec<(u64, u64)> = stats
        .ranges
        .iter()
        .filter(|r| r.as_str() != "bytes=0-0")
        .take(8)
        .map(|r| {
            let (a, b) = r.trim_start_matches("bytes=").split_once('-').unwrap();
            (a.parse().unwrap(), b.parse().unwrap())
        })
        .collect();
    opening.sort();
    assert_eq!(opening.len(), 8);
    assert_eq!(opening[0].0, 0);
    assert_eq!(opening[7].1, size - 1);
    for pair in opening.windows(2) {
        assert_eq!(pair[0].1 + 1, pair[1].0, "opening ranges do not tile: {opening:?}");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_workers_steal_from_the_busiest() {
    let server = TestServer::start().await;
    let size = 6 * MIB;
    server.route("/steal.bin", Route::pattern(size, 2).throttle(3 * MIB));
    let dir = scratch("steal");
    let dest = dir.join("steal.bin");

    // Three workers split the file 1/4, 1/4, 1/2. The two quarter-owners
    // finish first and must come back for the big one's tail.
    let outcome = download(&server.url("/steal.bin"), &dest, tuning(3)).await;
    assert_completed(outcome, &dest, 2, size);

    let stats = server.stats("/steal.bin");
    let payload_requests = stats.ranges.iter().filter(|r| r.as_str() != "bytes=0-0").count();
    assert!(payload_requests > 3, "no steal happened: {:?}", stats.ranges);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_then_resume_continues_where_it_stopped() {
    let server = TestServer::start().await;
    let size = 8 * MIB;
    server.route("/pause.bin", Route::pattern(size, 3).throttle(2 * MIB).etag("\"v1\""));
    let dir = scratch("pause");
    let dest = dir.join("pause.bin");
    let url = server.url("/pause.bin");

    let job = start(&url, &dest, tuning(4), Arc::new(Limiter::default())).await.unwrap();
    wait_for(&job.progress, 2 * MIB).await;
    job.cancel.cancel();
    assert!(matches!(job.handle.await.unwrap(), Outcome::Stopped));

    let paused_at = job.progress.downloaded();
    assert!(paused_at >= 2 * MIB && paused_at < size);
    assert!(part_path(&dest).exists() && meta_path(&dest).exists());
    assert_eq!(std::fs::metadata(part_path(&dest)).unwrap().len(), size, "the file is pre-allocated");
    assert!(!dest.exists());
    let sent_before = server.stats("/pause.bin").bytes_sent;

    let outcome = download(&url, &dest, tuning(4)).await;
    assert_completed(outcome, &dest, 3, size);

    let resent = server.stats("/pause.bin").bytes_sent - sent_before;
    let expected = size - paused_at;
    // A steal abandons whatever the server had in flight on the victim's
    // connection, so a little more than the missing bytes goes over the wire.
    assert!(resent <= expected + expected / 4, "resume re-fetched {resent} bytes, only {expected} were missing");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn survives_a_crash_between_checkpoints() {
    let server = TestServer::start().await;
    let size = 8 * MIB;
    server.route("/crash.bin", Route::pattern(size, 4).throttle(2 * MIB));
    let dir = scratch("crash");
    let dest = dir.join("crash.bin");
    let url = server.url("/crash.bin");

    let job = start(&url, &dest, tuning(4), Arc::new(Limiter::default())).await.unwrap();
    wait_for(&job.progress, 3 * MIB).await;
    // No goodbye: the task is killed as a power cut would kill the process,
    // so only the periodic checkpoints exist.
    job.handle.abort();
    let _ = job.handle.await;

    let meta = siphon_core::disk::ResumeMeta::load(&meta_path(&dest)).expect("a checkpoint must exist");
    assert!(meta.downloaded() > 0 && meta.downloaded() <= job.progress.downloaded());

    let outcome = download(&url, &dest, tuning(4)).await;
    assert_completed(outcome, &dest, 4, size);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_expired_link_keeps_the_data_and_resumes_on_a_fresh_one() {
    let server = TestServer::start().await;
    let size = 8 * MIB;
    // An expired link is only noticed when a new request is made. The server
    // cuts every response short so that connections have to come back.
    server.route("/signed.bin", Route::pattern(size, 5).throttle(2 * MIB).token("first").drop_after(700 * 1024));
    let dir = scratch("expired");
    let dest = dir.join("signed.bin");

    let job = start(&server.url("/signed.bin?token=first"), &dest, tuning(4), Arc::new(Limiter::default()))
        .await
        .unwrap();
    wait_for(&job.progress, MIB).await;
    // The signature expires mid-transfer. Connections already streaming keep
    // going on a real CDN too; the next range request is what gets refused.
    server.update("/signed.bin", |r| r.token = Some("second".into()));

    match job.handle.await.unwrap() {
        Outcome::Expired { code } => assert_eq!(code, 403),
        other => panic!("expected the link to expire, got {other:?}"),
    }
    let kept = job.progress.downloaded();
    assert!(kept >= MIB);
    assert!(part_path(&dest).exists() && meta_path(&dest).exists(), "partial data must survive an expired link");

    let sent_before = server.stats("/signed.bin").bytes_sent;
    let outcome = download(&server.url("/signed.bin?token=second"), &dest, tuning(4)).await;
    assert_completed(outcome, &dest, 5, size);
    let resent = server.stats("/signed.bin").bytes_sent - sent_before;
    assert!(resent <= (size - kept) + (size - kept) / 4, "the fresh link re-fetched data that was already on disk");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_gone_link_is_reported_as_expired() {
    let server = TestServer::start().await;
    server.route("/gone.bin", Route::pattern(MIB, 6).token("x").denied_status(410));
    let dir = scratch("gone");
    match download(&server.url("/gone.bin?token=stale"), &dir.join("gone.bin"), tuning(4)).await {
        Outcome::Failed(Error::Expired(410)) => {}
        other => panic!("{other:?}"),
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_redirect_target_is_resolved_again_from_the_original_link() {
    let server = TestServer::start().await;
    let size = 8 * MIB;
    // An expired link is only noticed when a new request is made. The server
    // cuts every response short so that connections have to come back.
    server.route("/cdn/file.bin", Route::pattern(size, 7).throttle(2 * MIB).token("a").drop_after(700 * 1024));
    server.route("/get", Route::redirect("/cdn/file.bin?token=a"));
    let dir = scratch("redirect");
    let dest = dir.join("file.bin");

    let job = start(&server.url("/get"), &dest, tuning(3), Arc::new(Limiter::default())).await.unwrap();
    wait_for(&job.progress, MIB).await;
    // The signed target rotates; the stable link now points at the new one.
    server.update("/cdn/file.bin", |r| r.token = Some("b".into()));
    server.update("/get", |r| r.redirect = Some("/cdn/file.bin?token=b".into()));

    let outcome = job.handle.await.unwrap();
    assert_completed(outcome, &dest, 7, size);
    assert!(server.stats("/get").requests >= 2, "the original link must have been followed again");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn falls_back_to_one_stream_without_range_support() {
    let server = TestServer::start().await;
    let size = 3 * MIB + 7;
    server.route("/plain.bin", Route::pattern(size, 8).no_ranges());
    let dir = scratch("noranges");
    let dest = dir.join("plain.bin");

    let outcome = download(&server.url("/plain.bin"), &dest, tuning(8)).await;
    assert_completed(outcome, &dest, 8, size);
    let stats = server.stats("/plain.bin");
    assert_eq!(stats.requests, 1, "the probe response itself must be used as the download");
    assert_eq!(stats.peak_connections, 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downloads_a_file_of_unknown_size() {
    let server = TestServer::start().await;
    let size = 2 * MIB + 99;
    server.route("/chunked.bin", Route::pattern(size, 9).no_length());
    let dir = scratch("chunked");
    let dest = dir.join("chunked.bin");

    let job = start(&server.url("/chunked.bin"), &dest, tuning(8), Arc::new(Limiter::default())).await.unwrap();
    let outcome = job.handle.await.unwrap();
    assert_completed(outcome, &dest, 9, size);
    assert!(job.progress.segments().is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retries_through_dropped_connections() {
    let server = TestServer::start().await;
    let size = 4 * MIB;
    // Every response dies after 300 KiB.
    server.route("/flaky.bin", Route::pattern(size, 10).drop_after(300 * 1024));
    let dir = scratch("flaky");
    let dest = dir.join("flaky.bin");

    let outcome = download(&server.url("/flaky.bin"), &dest, tuning(4)).await;
    assert_completed(outcome, &dest, 10, size);
    assert!(server.stats("/flaky.bin").requests > 10);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adapts_to_a_server_that_caps_connections() {
    let server = TestServer::start().await;
    let size = 6 * MIB;
    server.route("/capped.bin", Route::pattern(size, 11).max_connections(2).throttle(6 * MIB));
    let dir = scratch("capped");
    let dest = dir.join("capped.bin");

    let outcome = download(&server.url("/capped.bin"), &dest, tuning(8)).await;
    assert_completed(outcome, &dest, 11, size);
    assert!(server.stats("/capped.bin").statuses.contains(&503), "the cap was never hit");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_changed_file_is_never_stitched_onto_old_bytes() {
    let server = TestServer::start().await;
    let size = 8 * MIB;
    server.route("/changing.bin", Route::pattern(size, 12).throttle(2 * MIB).etag("\"v1\""));
    let dir = scratch("changed");
    let dest = dir.join("changing.bin");
    let url = server.url("/changing.bin");

    let job = start(&url, &dest, tuning(4), Arc::new(Limiter::default())).await.unwrap();
    wait_for(&job.progress, MIB).await;
    job.cancel.cancel();
    assert!(matches!(job.handle.await.unwrap(), Outcome::Stopped));

    // Same size, different content, new ETag.
    server.route("/changing.bin", Route::pattern(size, 999).etag("\"v2\""));
    match download(&url, &dest, tuning(4)).await {
        Outcome::Failed(Error::Changed) => {}
        other => panic!("expected the change to be detected, got {other:?}"),
    }
    assert!(!dest.exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_replaced_mid_download_is_detected() {
    let server = TestServer::start().await;
    let size = 8 * MIB;
    // Responses are cut short so that connections come back and meet the new file.
    server.route("/swap.bin", Route::pattern(size, 13).throttle(2 * MIB).etag("\"v1\"").drop_after(700 * 1024));
    let dir = scratch("swap");
    let dest = dir.join("swap.bin");

    let job = start(&server.url("/swap.bin"), &dest, tuning(3), Arc::new(Limiter::default())).await.unwrap();
    wait_for(&job.progress, MIB).await;
    server.update("/swap.bin", |r| {
        r.etag = Some("\"v2\"".into());
        r.content = siphon_testserver::Content::Pattern { size, seed: 77 };
    });
    match job.handle.await.unwrap() {
        Outcome::Failed(Error::Changed) => {}
        other => panic!("{other:?}"),
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn empty_and_tiny_files() {
    let server = TestServer::start().await;
    server.route("/empty.bin", Route::pattern(0, 14));
    server.route("/one.bin", Route::pattern(1, 15));
    let dir = scratch("tiny");

    let empty = dir.join("empty.bin");
    assert_completed(download(&server.url("/empty.bin"), &empty, tuning(8)).await, &empty, 14, 0);
    let one = dir.join("one.bin");
    assert_completed(download(&server.url("/one.bin"), &one, tuning(8)).await, &one, 15, 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovers_from_a_server_that_starts_out_unavailable() {
    let server = TestServer::start().await;
    let size = 2 * MIB;
    server.route("/warm.bin", Route::pattern(size, 16));
    let dir = scratch("warm");
    let dest = dir.join("warm.bin");
    let url = server.url("/warm.bin");

    let source = Arc::new(Source::Http(HttpSource::new(client(), Request::new(&url)).unwrap()));
    let (probe, body) = source.probe().await.unwrap();
    // Trouble starts after the probe: the next few requests are refused.
    server.update("/warm.bin", |r| r.fail_first = 4);
    let plan = Plan { source, final_path: dest.clone(), probe, body, tuning: tuning(2) };
    let outcome = run(plan, Arc::new(Progress::default()), Arc::new(Limiter::default()), CancellationToken::new()).await;
    assert_completed(outcome, &dest, 16, size);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gives_up_cleanly_when_the_server_stays_down() {
    let server = TestServer::start().await;
    let size = 2 * MIB;
    server.route("/down.bin", Route::pattern(size, 17));
    let dir = scratch("down");
    let dest = dir.join("down.bin");

    let source = Arc::new(Source::Http(HttpSource::new(client(), Request::new(server.url("/down.bin"))).unwrap()));
    let (probe, body) = source.probe().await.unwrap();
    server.update("/down.bin", |r| r.fail_first = u32::MAX);
    let plan = Plan { source, final_path: dest.clone(), probe, body, tuning: tuning(2) };
    match run(plan, Arc::new(Progress::default()), Arc::new(Limiter::default()), CancellationToken::new()).await {
        Outcome::Failed(e) => assert!(e.is_transient(), "{e:?}"),
        other => panic!("{other:?}"),
    }
    assert!(meta_path(&dest).exists(), "a failed download keeps its checkpoint for a later retry");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_speed_limit_is_honoured() {
    let server = TestServer::start().await;
    let size = 3 * MIB;
    server.route("/limited.bin", Route::pattern(size, 18));
    let dir = scratch("limit");
    let dest = dir.join("limited.bin");

    let started = Instant::now();
    let job = start(&server.url("/limited.bin"), &dest, tuning(4), Arc::new(Limiter::new(2 * MIB))).await.unwrap();
    assert_completed(job.handle.await.unwrap(), &dest, 18, size);
    let secs = started.elapsed().as_secs_f64();
    assert!(secs >= 1.1, "3 MiB at 2 MiB/s finished in {secs:.2}s");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finishing_never_overwrites_an_existing_file() {
    let server = TestServer::start().await;
    let size = MIB;
    server.route("/dup.bin", Route::pattern(size, 19));
    let dir = scratch("dup");
    let dest = dir.join("dup.bin");

    let job = start(&server.url("/dup.bin"), &dest, tuning(2), Arc::new(Limiter::default())).await.unwrap();
    // Someone drops a file with the same name while the download runs.
    std::fs::write(&dest, b"precious").unwrap();
    match job.handle.await.unwrap() {
        Outcome::Completed { path, .. } => {
            assert_eq!(path, dir.join("dup (1).bin"));
            verify_file(&path, 19, size).unwrap();
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(std::fs::read(&dest).unwrap(), b"precious");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_probe_reads_name_size_and_range_support() {
    let server = TestServer::start().await;
    server.route(
        "/dl",
        Route::pattern(5 * MIB, 20)
            .disposition("attachment; filename*=UTF-8''R%C3%A9sum%C3%A9%202026.pdf")
            .content_type("application/pdf")
            .etag("\"abc\"")
            .last_modified("Wed, 21 Oct 2026 07:28:00 GMT"),
    );
    server.route("/stream", Route::pattern(1000, 21).no_length().content_type("video/mp4"));

    let source = HttpSource::new(client(), Request::new(server.url("/dl"))).unwrap();
    let (probe, body) = source.probe().await.unwrap();
    assert!(body.is_none());
    assert_eq!(probe.total, Some(5 * MIB));
    assert!(probe.resumable);
    assert_eq!(probe.file_name, "Résumé 2026.pdf");
    assert_eq!(probe.validators.etag.as_deref(), Some("\"abc\""));
    assert_eq!(probe.content_type.as_deref(), Some("application/pdf"));

    let source = HttpSource::new(client(), Request::new(server.url("/stream"))).unwrap();
    let (probe, body) = source.probe().await.unwrap();
    assert!(body.is_some(), "an unranged reply is handed over for reuse");
    assert_eq!(probe.total, None);
    assert!(!probe.resumable);
    assert_eq!(probe.file_name, "stream.mp4");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browser_headers_are_replayed_but_never_leak_across_origins() {
    let origin = TestServer::start().await;
    let cdn = TestServer::start().await;
    cdn.route("/blob", Route::pattern(MIB, 22));
    origin.route("/get", Route::redirect(cdn.url("/blob")));
    origin.route("/direct", Route::pattern(MIB, 23));

    let mut request = Request::new(origin.url("/direct")).with_referer("https://site.example/page");
    request.set_header("Cookie", "session=abc");
    request.set_header("User-Agent", "TestBrowser/1.0");
    request.set_header("Range", "bytes=5-6");
    request.set_header("Host", "evil.example");
    let source = HttpSource::new(client(), request.clone()).unwrap();
    source.probe().await.unwrap();
    let seen = origin.stats("/direct").last_headers;
    assert_eq!(seen.get("cookie").map(String::as_str), Some("session=abc"));
    assert_eq!(seen.get("user-agent").map(String::as_str), Some("TestBrowser/1.0"));
    assert_eq!(seen.get("referer").map(String::as_str), Some("https://site.example/page"));
    assert_eq!(seen.get("range").map(String::as_str), Some("bytes=0-0"), "a captured Range must not win");
    assert_ne!(seen.get("host").map(String::as_str), Some("evil.example"));
    assert_eq!(seen.get("accept-encoding").map(String::as_str), Some("identity"));

    request.url = origin.url("/get");
    let source = HttpSource::new(client(), request).unwrap();
    let (probe, _) = source.probe().await.unwrap();
    assert_eq!(probe.final_url, cdn.url("/blob"));
    // Pinned requests go straight to the other origin and must not carry the cookie.
    drop(source.open(0, MIB, MIB).await.unwrap());
    let seen = cdn.stats("/blob").last_headers;
    assert_eq!(seen.get("cookie"), None, "a cookie crossed origins");
    assert_eq!(seen.get("user-agent").map(String::as_str), Some("TestBrowser/1.0"));
}

/// Raw throughput on loopback, where the network is not the limit. Run with
/// `cargo test --release -p siphon-core --test transfer loopback_throughput -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "a benchmark, not a correctness test"]
async fn loopback_throughput() {
    let server = TestServer::start().await;
    let size = 2048 * MIB;
    server.route("/bench.bin", Route::pattern(size, 99));
    let dir = scratch("bench");
    let dest = dir.join("bench.bin");

    for connections in [1usize, 8, 16] {
        let _ = std::fs::remove_file(&dest);
        let started = Instant::now();
        let job = start(&server.url("/bench.bin"), &dest, Tuning { connections, ..Tuning::default() }, Arc::new(Limiter::default())).await.unwrap();
        match job.handle.await.unwrap() {
            Outcome::Completed { bytes, .. } => assert_eq!(bytes, size),
            other => panic!("{other:?}"),
        }
        let secs = started.elapsed().as_secs_f64();
        println!("{connections:>2} connections: {:.0} MiB/s ({secs:.2}s for {} MiB)", size as f64 / MIB as f64 / secs, size / MIB);
    }
    verify_file(&dest, 99, size).unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}
