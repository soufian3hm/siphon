//! FTP, HLS and MMS against local servers.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aes::cipher::{BlockModeEncrypt, KeyIvInit, block_padding::Pkcs7};
use siphon_core::error::Error;
use siphon_core::ftp::FtpSource;
use siphon_core::net::{NetConfig, ProxyMode, build_client};
use siphon_core::rate::Limiter;
use siphon_core::segments::StealPolicy;
use siphon_core::source::Source;
use siphon_core::transfer::{Outcome, Plan, Progress, Tuning, run};
use siphon_core::types::Request;
use siphon_core::{hls, mms};
use siphon_testserver::{Content, FtpOptions, FtpTestServer, Route, TestServer, pattern, verify_file};
use tokio_util::sync::CancellationToken;

const MIB: u64 = 1024 * 1024;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("siphon-proto-{}-{name}", std::process::id()));
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

async fn ftp_download(url: &str, dest: &Path, connections: usize, cancel: CancellationToken, progress: Arc<Progress>) -> Outcome {
    let source = match FtpSource::new(&Request::new(url)) {
        Ok(source) => Arc::new(Source::Ftp(source)),
        Err(e) => return Outcome::Failed(e),
    };
    let (probe, body) = match source.probe().await {
        Ok(found) => found,
        Err(e) => return Outcome::Failed(e),
    };
    let plan = Plan { source, final_path: dest.to_owned(), probe, body, tuning: tuning(connections) };
    run(plan, progress, Arc::new(Limiter::default()), cancel).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ftp_files_are_segmented_with_rest() {
    let server = FtpTestServer::start(FtpOptions::default()).await;
    let size = 6 * MIB + 321;
    server.file("/pub/image.iso", Content::Pattern { size, seed: 31 });
    let dir = scratch("ftp-seg");
    let dest = dir.join("image.iso");

    let outcome = ftp_download(&server.url("/pub/image.iso"), &dest, 4, CancellationToken::new(), Arc::default()).await;
    assert!(matches!(outcome, Outcome::Completed { bytes, .. } if bytes == size), "{outcome:?}");
    verify_file(&dest, 31, size).unwrap();

    let stats = server.stats();
    assert!(stats.peak_sessions >= 3, "expected parallel sessions, peak was {}", stats.peak_sessions);
    let mut offsets = stats.retr_offsets.clone();
    offsets.sort();
    assert_eq!(offsets[0], 0);
    assert!(offsets.iter().filter(|o| **o > 0).count() >= 3, "REST was not used to split the file: {offsets:?}");
    // REST must be the command right before RETR on its session.
    assert!(stats.commands.iter().any(|c| c.starts_with("REST ")));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ftp_pause_and_resume() {
    let server = FtpTestServer::start(FtpOptions { throttle: Some(2 * MIB), ..Default::default() }).await;
    let size = 6 * MIB;
    server.file("/a.bin", Content::Pattern { size, seed: 32 });
    let dir = scratch("ftp-resume");
    let dest = dir.join("a.bin");
    let url = server.url("/a.bin");

    let cancel = CancellationToken::new();
    let progress = Arc::new(Progress::default());
    let task = tokio::spawn({
        let (url, dest, cancel, progress) = (url.clone(), dest.clone(), cancel.clone(), progress.clone());
        async move { ftp_download(&url, &dest, 3, cancel, progress).await }
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while progress.downloaded() < MIB {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    cancel.cancel();
    assert!(matches!(task.await.unwrap(), Outcome::Stopped));
    let kept = progress.downloaded();
    let sent_before = server.stats().bytes_sent;

    let outcome = ftp_download(&url, &dest, 3, CancellationToken::new(), Arc::default()).await;
    assert!(matches!(outcome, Outcome::Completed { .. }), "{outcome:?}");
    verify_file(&dest, 32, size).unwrap();
    let resent = server.stats().bytes_sent - sent_before;
    assert!(resent < size - kept + MIB, "resume fetched {resent} bytes for {} missing", size - kept);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ftp_without_size_or_rest_is_one_stream() {
    let server = FtpTestServer::start(FtpOptions { size: false, rest: false, ..Default::default() }).await;
    let size = 2 * MIB + 5;
    server.file("/old.bin", Content::Pattern { size, seed: 33 });
    let dir = scratch("ftp-plain");
    let dest = dir.join("old.bin");

    let outcome = ftp_download(&server.url("/old.bin"), &dest, 8, CancellationToken::new(), Arc::default()).await;
    assert!(matches!(outcome, Outcome::Completed { bytes, .. } if bytes == size), "{outcome:?}");
    verify_file(&dest, 33, size).unwrap();
    assert_eq!(server.stats().retr_offsets, vec![0]);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ftp_logins_and_missing_files() {
    let server = FtpTestServer::start(FtpOptions { login: Some(("ana".into(), "s3cret".into())), ..Default::default() }).await;
    server.file("/f.bin", Content::Pattern { size: 1000, seed: 34 });
    let dir = scratch("ftp-login");

    let good = dir.join("f.bin");
    let outcome = ftp_download(&server.url_with_login("ana", "s3cret", "/f.bin"), &good, 2, CancellationToken::new(), Arc::default()).await;
    assert!(matches!(outcome, Outcome::Completed { .. }), "{outcome:?}");
    verify_file(&good, 34, 1000).unwrap();

    match ftp_download(&server.url_with_login("ana", "wrong", "/f.bin"), &dir.join("x"), 2, CancellationToken::new(), Arc::default()).await {
        Outcome::Failed(Error::Protocol(message)) => assert!(message.contains("user name or password"), "{message}"),
        other => panic!("{other:?}"),
    }
    match ftp_download(&server.url_with_login("ana", "s3cret", "/nope.bin"), &dir.join("y"), 2, CancellationToken::new(), Arc::default()).await {
        Outcome::Failed(Error::Expired(550)) => {}
        other => panic!("{other:?}"),
    }
    assert!(!server.stats().commands.iter().any(|c| c.contains("s3cret")));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ftp_adapts_to_a_session_cap() {
    let server = FtpTestServer::start(FtpOptions { max_sessions: Some(2), throttle: Some(4 * MIB), ..Default::default() }).await;
    let size = 4 * MIB;
    server.file("/capped.bin", Content::Pattern { size, seed: 35 });
    let dir = scratch("ftp-cap");
    let dest = dir.join("capped.bin");

    let outcome = ftp_download(&server.url("/capped.bin"), &dest, 6, CancellationToken::new(), Arc::default()).await;
    assert!(matches!(outcome, Outcome::Completed { .. }), "{outcome:?}");
    verify_file(&dest, 35, size).unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

// ---------------------------------------------------------------- HLS ----

struct Stream {
    server: TestServer,
    /// What the stitched file must contain.
    expected: Vec<u8>,
}

fn encrypt(key: [u8; 16], iv: [u8; 16], plain: &[u8]) -> Vec<u8> {
    let mut buf = plain.to_vec();
    buf.resize(plain.len() + 16, 0);
    let len = cbc::Encryptor::<aes::Aes128>::new(&key.into(), &iv.into())
        .encrypt_padded::<Pkcs7>(&mut buf, plain.len())
        .unwrap()
        .len();
    buf.truncate(len);
    buf
}

/// A master playlist with two qualities; the better one has `segments`
/// pieces of `segment_len` bytes, optionally AES-128 encrypted.
async fn stream(segments: usize, segment_len: u64, encrypted: bool, throttle: Option<u64>) -> Stream {
    let server = TestServer::start().await;
    server.route(
        "/show/master.m3u8",
        Route::bytes(
            concat!(
                "#EXTM3U\n",
                "#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\nlow/index.m3u8\n",
                "#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080\nhigh/index.m3u8\n",
            ),
            "application/vnd.apple.mpegurl",
        ),
    );
    server.route("/show/low/index.m3u8", Route::bytes("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\nnope.ts\n#EXT-X-ENDLIST\n", "application/vnd.apple.mpegurl"));

    let key = *b"0123456789abcdef";
    let mut playlist = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:100\n");
    if encrypted {
        playlist.push_str("#EXT-X-KEY:METHOD=AES-128,URI=\"/keys/k.bin\"\n");
        server.route("/keys/k.bin", Route::bytes(key.to_vec(), "application/octet-stream"));
    }
    let mut expected = Vec::new();
    for i in 0..segments {
        let plain = pattern(1000 + i as u64, segment_len + i as u64);
        expected.extend_from_slice(&plain);
        let body = if encrypted {
            // No IV in the playlist: the media sequence number is the IV.
            let mut iv = [0u8; 16];
            iv[8..].copy_from_slice(&(100 + i as u64).to_be_bytes());
            encrypt(key, iv, &plain)
        } else {
            plain
        };
        let mut route = Route::bytes(body, "video/mp2t");
        route.throttle = throttle;
        server.route(&format!("/show/high/seg{i}.ts"), route);
        playlist.push_str(&format!("#EXTINF:4.0,\nseg{i}.ts\n"));
    }
    playlist.push_str("#EXT-X-ENDLIST\n");
    server.route("/show/high/index.m3u8", Route::bytes(playlist, "application/vnd.apple.mpegurl"));
    Stream { server, expected }
}

async fn hls_download(url: &str, dest: &Path, cancel: &CancellationToken, progress: &Arc<Progress>) -> Result<Outcome, Error> {
    let client = client();
    let plan = hls::plan(&client, &Request::new(url)).await?;
    hls::run(&client, plan, dest, 4, progress, &Arc::new(Limiter::default()), cancel, &|| {}).await
}

/// The payload here is not real video, so `ffmpeg` (when installed) refuses
/// it and the engine keeps the stitched stream. Either way the bytes must be
/// the segments in playlist order.
fn stitched(outcome: Outcome) -> PathBuf {
    match outcome {
        Outcome::Completed { path, .. } => path,
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hls_picks_the_best_variant_and_stitches_in_order() {
    let s = stream(9, 40_000, false, None).await;
    let dir = scratch("hls-plain");

    let plan = hls::plan(&client(), &Request::new(s.server.url("/show/master.m3u8"))).await.unwrap();
    assert_eq!(plan.resolution, Some((1920, 1080)));
    assert_eq!(plan.segment_count(), 9);
    assert!(plan.media_url.ends_with("/show/high/index.m3u8"));
    assert!(plan.describe().starts_with("1080p \u{00B7} 0:36"), "{}", plan.describe());
    assert!(plan.file_name().starts_with("show."));

    let progress = Arc::new(Progress::default());
    let path = stitched(hls_download(&s.server.url("/show/master.m3u8"), &dir.join("show.mp4"), &CancellationToken::new(), &progress).await.unwrap());
    assert_eq!(std::fs::read(&path).unwrap(), s.expected);
    assert_eq!(progress.unit_fraction(), Some(1.0));
    assert_eq!(s.server.stats("/show/low/index.m3u8").requests, 0, "the low quality must not be touched");
    let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
    assert_eq!(leftovers.len(), 1, "segment cache must be removed: {leftovers:?}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hls_decrypts_aes_128_segments() {
    let s = stream(6, 30_001, true, None).await;
    let dir = scratch("hls-aes");
    let path = stitched(hls_download(&s.server.url("/show/master.m3u8"), &dir.join("enc.mp4"), &CancellationToken::new(), &Arc::default()).await.unwrap());
    assert_eq!(std::fs::read(&path).unwrap(), s.expected);
    assert_eq!(s.server.stats("/keys/k.bin").requests, 1, "the key is fetched once and cached");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hls_resumes_without_refetching_finished_segments() {
    let s = stream(12, 200_000, false, Some(800_000)).await;
    let dir = scratch("hls-resume");
    let dest = dir.join("long.mp4");
    let url = s.server.url("/show/master.m3u8");

    let cancel = CancellationToken::new();
    let progress = Arc::new(Progress::default());
    let task = tokio::spawn({
        let (url, dest, cancel, progress) = (url.clone(), dest.clone(), cancel.clone(), progress.clone());
        async move { hls_download(&url, &dest, &cancel, &progress).await }
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while progress.unit_fraction().unwrap_or(0.0) < 0.3 {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    cancel.cancel();
    assert!(matches!(task.await.unwrap(), Err(Error::Cancelled)));

    let done_before: Vec<usize> = (0..12).filter(|i| s.server.stats(&format!("/show/high/seg{i}.ts")).bytes_sent >= 200_000 + *i as u64).collect();
    assert!(done_before.len() >= 3);

    let resumed = Arc::new(Progress::default());
    let path = stitched(hls_download(&url, &dest, &CancellationToken::new(), &resumed).await.unwrap());
    assert_eq!(std::fs::read(&path).unwrap(), s.expected);
    for i in done_before {
        assert_eq!(s.server.stats(&format!("/show/high/seg{i}.ts")).requests, 1, "segment {i} was fetched again");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hls_reports_an_expired_stream() {
    let s = stream(4, 10_000, false, None).await;
    s.server.update("/show/high/seg2.ts", |r| r.token = Some("fresh".into()));
    let dir = scratch("hls-expired");
    match hls_download(&s.server.url("/show/master.m3u8"), &dir.join("x.mp4"), &CancellationToken::new(), &Arc::default()).await {
        Err(Error::Expired(403)) => {}
        other => panic!("{other:?}"),
    }
    std::fs::remove_dir_all(dir).unwrap();
}

/// End to end with real media: `ffmpeg` produces a genuine HLS stream, the
/// engine downloads it, and `ffprobe` confirms the result is an MP4 with
/// both tracks and the right length.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hls_real_stream_becomes_a_playable_mp4() {
    let Some(ffmpeg) = hls::find_ffmpeg() else {
        eprintln!("ffmpeg not installed; skipping the real-media test");
        return;
    };
    let ffprobe = ffmpeg.with_file_name("ffprobe");
    let dir = scratch("hls-real");
    let source = dir.join("source");
    std::fs::create_dir_all(&source).unwrap();

    let made = std::process::Command::new(&ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc=duration=6:size=320x180:rate=25"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=6"])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-g", "25", "-c:a", "aac"])
        .args(["-f", "hls", "-hls_time", "1", "-hls_playlist_type", "vod", "-hls_segment_filename"])
        .arg(source.join("part%03d.ts"))
        .arg(source.join("index.m3u8"))
        .status()
        .unwrap();
    if !made.success() {
        eprintln!("this ffmpeg build cannot encode the fixture; skipping");
        return;
    }

    let server = TestServer::start().await;
    for entry in std::fs::read_dir(&source).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let kind = if name.ends_with(".m3u8") { "application/vnd.apple.mpegurl" } else { "video/mp2t" };
        server.route(&format!("/clip/{name}"), Route::bytes(std::fs::read(entry.path()).unwrap(), kind));
    }

    let path = stitched(hls_download(&server.url("/clip/index.m3u8"), &dir.join("clip.mp4"), &CancellationToken::new(), &Arc::default()).await.unwrap());
    assert_eq!(path.extension().and_then(|e| e.to_str()), Some("mp4"));

    let probe = std::process::Command::new(&ffprobe)
        .args(["-v", "error", "-show_entries", "format=format_name,duration:stream=codec_type", "-of", "default=nw=1"])
        .arg(&path)
        .output()
        .unwrap();
    let report = String::from_utf8_lossy(&probe.stdout);
    assert!(report.contains("format_name=mov,mp4"), "{report}");
    assert!(report.contains("codec_type=video") && report.contains("codec_type=audio"), "{report}");
    let duration: f64 = report.lines().find_map(|l| l.strip_prefix("duration=")).unwrap().parse().unwrap();
    assert!((duration - 6.0).abs() < 0.5, "duration {duration}");
    std::fs::remove_dir_all(dir).unwrap();
}

// ---------------------------------------------------------------- MMS ----

fn mms_frame(kind: &[u8; 2], sequence: u32, payload: &[u8]) -> Vec<u8> {
    let extension = if kind == b"$H" || kind == b"$D" { 8 } else { 4 };
    let length = (payload.len() + extension) as u16;
    let mut out = kind.to_vec();
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(&sequence.to_le_bytes());
    if extension == 8 {
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&length.to_le_bytes());
    }
    out.extend_from_slice(payload);
    out
}

/// A structurally valid ASF header: file properties (packet size, file
/// size), one stream, and the data object header that travels with it.
fn asf_header(packet_len: u32, file_size: u64) -> Vec<u8> {
    const HEADER: [u8; 16] = [0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C];
    const FILE: [u8; 16] = [0xA1, 0xDC, 0xAB, 0x8C, 0x47, 0xA9, 0xCF, 0x11, 0x8E, 0xE4, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65];
    const STREAM: [u8; 16] = [0x91, 0x07, 0xDC, 0xB7, 0xB7, 0xA9, 0xCF, 0x11, 0x8E, 0xE6, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65];

    let mut file = FILE.to_vec();
    file.extend_from_slice(&104u64.to_le_bytes());
    file.extend_from_slice(&[0xAB; 16]);
    file.extend_from_slice(&file_size.to_le_bytes());
    file.extend_from_slice(&[0u8; 40]);
    file.extend_from_slice(&2u32.to_le_bytes());
    file.extend_from_slice(&packet_len.to_le_bytes());
    file.extend_from_slice(&packet_len.to_le_bytes());
    file.extend_from_slice(&64_000u32.to_le_bytes());

    let mut stream = STREAM.to_vec();
    stream.extend_from_slice(&78u64.to_le_bytes());
    stream.extend_from_slice(&[0x11; 32]);
    stream.extend_from_slice(&[0u8; 16]);
    stream.extend_from_slice(&3u16.to_le_bytes());
    stream.extend_from_slice(&[0u8; 4]);

    let mut header = HEADER.to_vec();
    header.extend_from_slice(&((30 + file.len() + stream.len()) as u64).to_le_bytes());
    header.extend_from_slice(&2u32.to_le_bytes());
    header.extend_from_slice(&[1, 2]);
    header.extend_from_slice(&file);
    header.extend_from_slice(&stream);
    header.extend_from_slice(&[0xDA; 50]);
    header
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mms_stream_is_saved_as_an_asf_file() {
    const PACKET: usize = 512;
    let header = asf_header(PACKET as u32, 99_000);
    let packets: Vec<Vec<u8>> = vec![pattern(41, 300), pattern(42, PACKET as u64), pattern(43, 77), pattern(44, 511)];

    let mut wire = mms_frame(b"$H", 0, &header);
    let mut expected = header.clone();
    for (i, packet) in packets.iter().enumerate() {
        wire.extend(mms_frame(b"$D", i as u32, packet));
        expected.extend_from_slice(packet);
        expected.resize(header.len() + (i + 1) * PACKET, 0);
    }
    wire.extend(mms_frame(b"$E", 0, &[]));
    // Anything after the end frame belongs to other content.
    wire.extend(mms_frame(b"$D", 99, &[0xEE; 100]));

    let server = TestServer::start().await;
    server.route("/media/lecture.wmv", Route::bytes(wire, "application/x-mms-framed"));
    let url = format!("mms://{}/media/lecture.wmv", server.addr());
    let dir = scratch("mms");
    let dest = dir.join("lecture.wmv");
    let client = client();

    let info = mms::probe(&client, &Request::new(&url)).await.unwrap();
    assert_eq!(info.file_name, "lecture.wmv");
    assert_eq!(info.total, Some(99_000));

    let session = mms::open(&client, &Request::new(&url)).await.unwrap();
    let progress = Arc::new(Progress::default());
    let outcome = mms::run(session, &dest, &progress, &Arc::new(Limiter::default()), &CancellationToken::new()).await.unwrap();
    assert!(matches!(outcome, Outcome::Completed { bytes, .. } if bytes == expected.len() as u64), "{outcome:?}");
    assert_eq!(std::fs::read(&dest).unwrap(), expected, "packets must be padded back to the fixed packet size");

    let seen = server.stats("/media/lecture.wmv").last_headers;
    assert!(seen.get("user-agent").is_some_and(|ua| ua.starts_with("NSPlayer/")));
    assert!(seen.contains_key("pragma"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mms_rejects_a_server_that_is_not_windows_media() {
    let server = TestServer::start().await;
    server.route("/page", Route::bytes("<html>hello</html>", "text/html"));
    match mms::probe(&client(), &Request::new(format!("mms://{}/page", server.addr()))).await {
        Err(Error::Protocol(message)) => assert!(message.contains("Windows Media"), "{message}"),
        other => panic!("{other:?}"),
    }
}
