//! A local server with a believable catalogue of files, for trying the
//! application by hand and for end-to-end checks with a browser.
//!
//! ```text
//! siphon-testserver [--port 8765] [--expire-after <seconds>]
//! ```

use std::time::Duration;

use siphon_testserver::{Route, TestServer, pattern, stored_zip};

const MIB: u64 = 1024 * 1024;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let port: u16 = value("--port").and_then(|p| p.parse().ok()).unwrap_or(8765);
    let expire_after: Option<u64> = value("--expire-after").and_then(|s| s.parse().ok());

    let server = match TestServer::bind(&format!("127.0.0.1:{port}")).await {
        Ok(server) => server,
        Err(e) => {
            eprintln!("could not listen on port {port}: {e}");
            std::process::exit(1);
        }
    };

    // (path, size, bytes per second per connection)
    let files: [(&str, u64, Option<u64>); 7] = [
        ("/releases/ubuntu-26.04-desktop-amd64.iso", 2400 * MIB, Some(3 * MIB / 2)),
        ("/video/Planet.Earth.S01E03.2160p.mkv", 1480 * MIB, Some(900 * 1024)),
        ("/apps/Blender-5.2-macos-arm64.dmg", 412 * MIB, Some(700 * 1024)),
        ("/audio/Hardcore.History.Ep72.mp3", 186 * MIB, Some(2 * MIB)),
        ("/docs/Annual-Report-2026.pdf", 14 * MIB, None),
        ("/images/wallpapers-5k.zip", 96 * MIB, None),
        ("/src/linux-7.1.tar.xz", 148 * MIB, Some(5 * MIB)),
    ];
    for (i, (path, size, throttle)) in files.iter().enumerate() {
        let mut route = Route::pattern(*size, 100 + i as u64).etag(&format!("\"demo-{i}\""));
        route.throttle = *throttle;
        server.route(path, route);
    }
    // A signed link that can be made to expire.
    server.route("/signed/research-dataset.tar.gz", Route::pattern(1900 * MIB, 200).throttle(1200 * 1024).token("alpha"));
    // A server that hides the size and cannot resume.
    server.route("/stream/live-recording.ts", Route::pattern(64 * MIB, 201).throttle(600 * 1024).no_length().content_type("video/mp2t"));
    // A file with a name only the header knows.
    server.route(
        "/download",
        Route::pattern(8 * MIB, 202).disposition("attachment; filename=\"Quarterly Figures (final).xlsx\"").content_type("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
    );

    // A real archive, so its contents can be listed before downloading it.
    let kit = stored_zip(&[
        ("Siphon press kit/", Vec::new()),
        ("Siphon press kit/Read me first.txt", pattern(400, 1200)),
        ("Siphon press kit/Fact sheet.pdf", pattern(2 * MIB, 4 * MIB)),
        ("Siphon press kit/logo/", Vec::new()),
        ("Siphon press kit/logo/Siphon mark (dark).png", pattern(401, 180 * 1024)),
        ("Siphon press kit/logo/Siphon mark (light).png", pattern(402, 176 * 1024)),
        ("Siphon press kit/logo/Siphon wordmark.svg", pattern(403, 9 * 1024)),
        ("Siphon press kit/screens/", Vec::new()),
        ("Siphon press kit/screens/01 Library.png", pattern(404, 1400 * 1024)),
        ("Siphon press kit/screens/02 Byte map.png", pattern(405, 1250 * 1024)),
        ("Siphon press kit/screens/03 Captured link.png", pattern(406, 990 * 1024)),
        ("Siphon press kit/screens/04 Queue.png", pattern(407, 1100 * 1024)),
        ("Siphon press kit/video/Walkthrough 4K.mp4", pattern(408, 38 * MIB)),
    ]);
    let mut kit_route = Route::bytes(kit, "application/zip").etag("\"kit-1\"");
    kit_route.throttle = Some(6 * MIB);
    server.route("/press/Siphon-press-kit.zip", kit_route);

    // A file behind HTTP Basic authentication (user "ada", password "lovelace").
    server.route("/members/Field-Recordings-Vol-3.flac", Route::pattern(220 * MIB, 210).throttle(2 * MIB).basic_auth("YWRhOmxvdmVsYWNl"));

    // A gallery with a second level, for the link grabber.
    let mut gallery = String::from("<!doctype html><meta charset=utf-8><title>Alpine survey 2026: photo archive</title><body style=\"font:15px system-ui;margin:48px\"><h1>Alpine survey 2026</h1>");
    for n in 1..=18 {
        let path = format!("/gallery/photos/alpine-{n:03}.jpg");
        server.route(&path, Route::pattern((2 + n % 5) * MIB, 500 + n).content_type("image/jpeg"));
        gallery.push_str(&format!("<a href=\"{path}\"><img src=\"/gallery/thumbs/alpine-{n:03}.webp\" alt=\"Ridge line, frame {n}\"></a>"));
        server.route(&format!("/gallery/thumbs/alpine-{n:03}.webp"), Route::pattern(24 * 1024, 600 + n).content_type("image/webp"));
    }
    gallery.push_str("<p><a href=\"/gallery/raw.html\">Raw files and maps</a> <a href=\"/gallery/about.html\">About the survey</a> <a href=\"/press/Siphon-press-kit.zip\">Press kit</a></p>");
    server.route("/gallery/", Route::bytes(gallery, "text/html; charset=utf-8"));
    let mut raw = String::from("<!doctype html><meta charset=utf-8><title>Raw files</title>");
    for (name, size) in [("alpine-raw-set-a.tar", 900), ("alpine-raw-set-b.tar", 870), ("survey-track.gpx", 1), ("elevation-model.tif", 310), ("field-notes.pdf", 6), ("drone-pass-01.mp4", 640), ("drone-pass-02.mp4", 712), ("ambient-ridge.flac", 88)] {
        let path = format!("/gallery/raw/{name}");
        server.route(&path, Route::pattern(size * MIB, 700 + size).throttle(3 * MIB));
        raw.push_str(&format!("<a href=\"{path}\">{name}</a> "));
    }
    server.route("/gallery/raw.html", Route::bytes(raw, "text/html; charset=utf-8"));
    server.route("/gallery/about.html", Route::bytes("<!doctype html><title>About</title><p>Nothing to download here.</p>", "text/html; charset=utf-8"));

    // A page with a real <video> element, for the in-page download button.
    server.route("/media/Product-Tour.mp4", Route::pattern(6 * MIB, 220).content_type("video/mp4").etag("\"tour-1\""));
    server.route(
        "/clip",
        Route::bytes(
            "<!doctype html><meta charset=utf-8><title>Product tour</title><body style=\"font:15px system-ui;margin:48px\">\
             <h1>Product tour</h1><video id=player src=\"/media/Product-Tour.mp4\" width=640 height=360 controls muted preload=auto style=\"background:#000;display:block\"></video>",
            "text/html; charset=utf-8",
        ),
    );
    // A feed of posts, each with a player that has no file address of its
    // own (it plays from memory, as players on large sites do) and a date
    // that links to the post. The files behind each post, in two sizes, are
    // what a video helper would report for it.
    let mut feed = String::from(
        "<!doctype html><meta charset=utf-8><title>Field notes: the feed</title>\
         <body style=\"font:15px system-ui;margin:0 auto;max-width:620px;padding:24px;background:#f4f4f5\">",
    );
    for (n, (author, words)) in [("Ada Lovelace", "First light over the ridge"), ("Grace Hopper", "The harbour at noon"), ("Katherine Johnson", "Launch day, from the roof")].iter().enumerate() {
        let n = n + 1;
        feed.push_str(&format!(
            "<article id=\"post-{n}\" style=\"background:#fff;border:1px solid #ddd;border-radius:10px;padding:16px;margin:0 0 20px\">\
             <header><a rel=\"author\" href=\"/people/{n}\">{author}</a> · <a href=\"/post/{n}\"><time datetime=\"2026-11-2{n}\">Nov 2{n}</time></a></header>\
             <p>{words}</p><video id=\"video-{n}\" width=\"560\" height=\"315\" muted style=\"background:#111;display:block;width:560px;height:315px\"></video></article>"
        ));
        server.route(&format!("/post/{n}"), Route::bytes(format!("<!doctype html><title>{words}</title><p>{words}</p>"), "text/html; charset=utf-8"));
        server.route(&format!("/media/feed/{n}/720.mp4"), Route::pattern((n as u64 + 2) * MIB, 240 + n as u64).content_type("video/mp4"));
        server.route(&format!("/media/feed/{n}/360.mp4"), Route::pattern(n as u64 * 300 * 1024, 250 + n as u64).content_type("video/mp4"));
    }
    feed.push_str("<script>for (const v of document.querySelectorAll('video')) v.src = URL.createObjectURL(new MediaSource());</script>");
    server.route("/feed", Route::bytes(feed, "text/html; charset=utf-8"));

    // A stand-in built the way X marks up its posts, for the rule by which
    // the extension finds a post's own address there. A test browser is told
    // to resolve that site's name to this server.
    let memory_players = "<script>for (const v of document.querySelectorAll('video')) v.src = URL.createObjectURL(new MediaSource());</script>";
    let player = |id: &str| format!("<div data-testid=\"videoPlayer\"><div data-testid=\"videoComponent\"><video id=\"{id}\" muted style=\"background:#111;display:block;width:480px;height:270px\"></video></div></div>");
    server.route(
        "/replica/x",
        Route::bytes(
            format!(
                "<!doctype html><meta charset=utf-8><title>Home / X</title><body style=\"font:15px system-ui;margin:24px\">\
                 <article data-testid=\"tweet\"><div data-testid=\"User-Name\"><span>Ada Lovelace</span> <a href=\"/ada/status/1790000000000000001\"><time datetime=\"2026-11-20\">Nov 20</time></a></div>\
                 <div data-testid=\"tweetText\">One clip</div>{}</article>\
                 <article data-testid=\"tweet\"><div data-testid=\"User-Name\"><span>Grace Hopper</span> <a href=\"/grace/status/1790000000000000002\"><time datetime=\"2026-11-21\">Nov 21</time></a></div>\
                 <div data-testid=\"tweetText\">Two clips in one post</div>{}{}</article>\
                 <article data-testid=\"tweet\"><div data-testid=\"User-Name\"><span>Promoted</span></div><div data-testid=\"tweetText\">An advertisement has no address of its own</div>{}</article>{memory_players}",
                player("x-1"),
                player("x-2a"),
                player("x-2b"),
                player("x-ad"),
            ),
            "text/html; charset=utf-8",
        ),
    );
    // A player that keeps its video inside a shadow tree, in a post whose
    // date links to the post: what a site built from web components does.
    server.route(
        "/replica/shadow",
        Route::bytes(
            "<!doctype html><meta charset=utf-8><title>Components</title><body style=\"font:15px system-ui;margin:24px\">\
             <article><header><a rel=\"author\" href=\"/people/9\">Mary Jackson</a> · <a href=\"/post/9\"><time datetime=\"2026-11-23\">Nov 23</time></a></header><p>Wind tunnel, first run</p>\
             <fancy-player id=\"shadow-player\" style=\"display:block;width:480px;height:270px\"></fancy-player></article>\
             <script>const outer = document.getElementById('shadow-player').attachShadow({ mode: 'open' }); const inner = document.createElement('fancy-surface'); outer.append(inner);\
             const v = document.createElement('video'); v.muted = true; v.style.cssText = 'background:#111;display:block;width:480px;height:270px'; v.src = URL.createObjectURL(new MediaSource()); inner.attachShadow({ mode: 'open' }).append(v);</script>",
            "text/html; charset=utf-8",
        ),
    );

    // Small files for the capture rules (Alt held, a type left alone, a site left alone).
    for name in ["kept-by-alt.txt", "kept-by-type.txt", "kept-by-site.bin"] {
        server.route(&format!("/rules/{name}"), Route::pattern(200 * 1024, 230));
    }

    let mut page = String::from(
        "<!doctype html><meta charset=utf-8><title>Siphon test files</title>\
         <style>body{font:15px/1.6 system-ui;margin:48px;max-width:720px}a{display:block;padding:6px 0}</style>\
         <h1>Siphon test files</h1><p>Click a link. With the extension installed, Siphon takes the download.</p>",
    );
    for (path, size, _) in &files {
        page.push_str(&format!("<a id=\"{}\" href=\"{path}\" download>{} <small>({} MB)</small></a>", path.rsplit('/').next().unwrap_or(""), &path[1..], size / MIB));
    }
    page.push_str("<a id=\"signed\" href=\"/signed/research-dataset.tar.gz?token=alpha\" download>signed/research-dataset.tar.gz <small>(expiring link)</small></a>");
    page.push_str("<a id=\"named\" href=\"/download\">download <small>(name from header)</small></a>");
    page.push_str("<a id=\"watch\" href=\"/watch\">watch <small>(a page that plays an HLS stream)</small></a>");
    page.push_str("<a id=\"kit\" href=\"/press/Siphon-press-kit.zip\" download>press/Siphon-press-kit.zip <small>(a real archive)</small></a>");
    page.push_str("<a id=\"members\" href=\"/members/Field-Recordings-Vol-3.flac\" download>members/Field-Recordings-Vol-3.flac <small>(needs the login ada / lovelace)</small></a>");
    page.push_str("<a id=\"gallery\" href=\"/gallery/\">gallery <small>(a page of photos, for the link grabber)</small></a>");
    page.push_str("<a id=\"clip\" href=\"/clip\">clip <small>(a page with a video element)</small></a>");
    page.push_str("<a id=\"feed\" href=\"/feed\">feed <small>(posts with a video each, for the button's quality menu)</small></a>");
    for name in ["kept-by-alt.txt", "kept-by-type.txt", "kept-by-site.bin"] {
        page.push_str(&format!("<a id=\"{name}\" href=\"/rules/{name}\" download>rules/{name}</a>"));
    }
    server.route("/", Route::bytes(page, "text/html; charset=utf-8"));

    // A small HLS stream and a page that requests it, the way a player does.
    server.route(
        "/hls/master.m3u8",
        Route::bytes(
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=640x360\n360p/index.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=4500000,RESOLUTION=1920x1080\n1080p/index.m3u8\n",
            "application/vnd.apple.mpegurl",
        ),
    );
    for quality in ["360p", "1080p"] {
        let mut playlist = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n");
        for i in 0..8 {
            playlist.push_str(&format!("#EXTINF:6.0,\nseg{i}.ts\n"));
            let mut segment = Route::pattern(700 * 1024, 300 + i).content_type("video/mp2t");
            segment.throttle = Some(900 * 1024);
            server.route(&format!("/hls/{quality}/seg{i}.ts"), segment);
        }
        playlist.push_str("#EXT-X-ENDLIST\n");
        server.route(&format!("/hls/{quality}/index.m3u8"), Route::bytes(playlist, "application/vnd.apple.mpegurl"));
    }
    server.route(
        "/watch",
        Route::bytes(
            "<!doctype html><meta charset=utf-8><title>Big Buck Bunny (trailer)</title><body style=\"font:15px system-ui;margin:48px\">\
             <h1>Big Buck Bunny (trailer)</h1><p id=state>loading stream\u{2026}</p>\
             <script>fetch('/hls/master.m3u8').then(r=>r.text()).then(()=>fetch('/hls/1080p/index.m3u8')).then(()=>{document.getElementById('state').textContent='stream requested'})</script>",
            "text/html; charset=utf-8",
        ),
    );

    println!("serving on {}", server.url("/"));
    if let Some(secs) = expire_after {
        tokio::time::sleep(Duration::from_secs(secs)).await;
        server.update("/signed/research-dataset.tar.gz", |r| r.token = Some("beta".into()));
        println!("the signed link expired; the fresh one is {}", server.url("/signed/research-dataset.tar.gz?token=beta"));
    }
    std::future::pending::<()>().await;
}
