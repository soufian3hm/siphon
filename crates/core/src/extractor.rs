//! Video pages: sites that do not hand out a file address, only a page with
//! a player on it.
//!
//! The address of such a page is the download's identity. What the player
//! would fetch is found out by `yt-dlp`, when it is installed: it is asked
//! for a description of the page (`-J`) and nothing else. The streams it
//! lists are then downloaded by this engine like any other file, video and
//! sound separately, and joined by `ffmpeg`.
//!
//! Nothing here deciphers or decrypts anything. Streams the site protects
//! with DRM are not listed by the resolver and are not touched.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::error::{Error, Result};

/// The longest a page description may take.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(90);
/// Requests are kept to this size unless the resolver says otherwise:
/// larger ones are slowed down by the hosts in question.
pub const DEFAULT_REQUEST_SIZE: u64 = 10 * 1024 * 1024;
/// The id of the sound-only choice.
pub const AUDIO_ONLY: &str = "audio";

/// Hosts whose watch pages are resolved through the helper.
fn is_youtube(host: &str) -> bool {
    let host = host.trim_start_matches("www.").trim_start_matches("m.").trim_start_matches("music.");
    host == "youtube.com" || host == "youtube-nocookie.com"
}

/// Whether an address is a video page rather than a file.
pub fn is_page(url: &str) -> bool {
    let Ok(url) = url::Url::parse(url.trim()) else { return false };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let path = url.path();
    if host == "youtu.be" {
        return path.len() > 1;
    }
    if !is_youtube(&host) {
        return false;
    }
    (path == "/watch" && url.query_pairs().any(|(key, value)| key == "v" && !value.is_empty()))
        || ["/shorts/", "/embed/", "/live/", "/v/"].iter().any(|prefix| path.starts_with(prefix) && path.len() > prefix.len())
}

/// The page address with everything but the video itself removed (playlist
/// position, start time, tracking), so the same video is always the same
/// download.
pub fn canonical(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url.trim()) else { return url.trim().to_owned() };
    if parsed.path() != "/watch" {
        let mut bare = parsed.clone();
        bare.set_query(None);
        bare.set_fragment(None);
        return bare.to_string();
    }
    let video = parsed.query_pairs().find(|(key, _)| key == "v").map(|(_, value)| value.into_owned()).unwrap_or_default();
    let mut bare = parsed.clone();
    bare.set_fragment(None);
    bare.query_pairs_mut().clear().append_pair("v", &video);
    bare.to_string()
}

/// One stream as the resolver describes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Format {
    pub id: String,
    pub ext: String,
    /// `None` when the stream has no picture.
    pub vcodec: Option<String>,
    /// `None` when the stream has no sound.
    pub acodec: Option<String>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    /// Total bitrate in kbit/s.
    pub bitrate: Option<f64>,
    pub size: Option<u64>,
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// The largest range to ask for in one request.
    pub request_size: Option<u64>,
    /// Higher is closer to the original sound track.
    pub language_preference: i64,
}

impl Format {
    fn has_video(&self) -> bool {
        self.vcodec.is_some()
    }

    fn has_audio(&self) -> bool {
        self.acodec.is_some()
    }

    fn codec_family(&self) -> Codec {
        match self.vcodec.as_deref() {
            Some(c) if c.starts_with("avc1") || c.starts_with("h264") => Codec::H264,
            Some(c) if c.starts_with("av01") => Codec::Av1,
            Some(c) if c.starts_with("vp9") || c.starts_with("vp09") => Codec::Vp9,
            _ => Codec::Other,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Codec {
    // Ordered by how widely the result plays: best first.
    H264,
    Av1,
    Vp9,
    Other,
}

impl Codec {
    fn label(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::Av1 => "AV1",
            Codec::Vp9 => "VP9",
            Codec::Other => "video",
        }
    }
}

/// What a page offers.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolved {
    pub title: String,
    pub duration: Option<u64>,
    pub live: bool,
    pub formats: Vec<Format>,
}

/// One choice the user can make: a picture quality, or sound only.
#[derive(Clone, Debug, PartialEq)]
pub struct Variant {
    /// The height as text (`"1080"`), or [`AUDIO_ONLY`]. Stable across
    /// resolutions of the same page, so a stored choice can be found again.
    pub id: String,
    /// `"1080p60"`.
    pub label: String,
    /// `"MP4 · H.264"`.
    pub detail: String,
    pub size: Option<u64>,
    pub video: Option<Format>,
    pub audio: Option<Format>,
    /// Extension of the finished file.
    pub container: &'static str,
}

impl Variant {
    /// The streams to fetch, picture first.
    pub fn stages(&self) -> Vec<(&'static str, &Format)> {
        let mut stages = Vec::new();
        if let Some(video) = &self.video {
            stages.push(("video", video));
        }
        if let Some(audio) = &self.audio {
            stages.push(("audio", audio));
        }
        stages
    }

    /// Two streams have to be joined; one is the file as it is.
    pub fn needs_joining(&self) -> bool {
        self.video.as_ref().is_some_and(|v| !v.has_audio()) && self.audio.is_some()
    }
}

/// The quality choices a page offers, best picture first, sound only last.
pub fn variants(resolved: &Resolved) -> Vec<Variant> {
    // Only streams that are plain files with a known size can be fetched in
    // ranges. Compressed-dynamics duplicates of the sound are left out.
    let usable: Vec<&Format> = resolved.formats.iter().filter(|f| f.size.is_some() && !f.url.is_empty() && !f.id.contains("drc")).collect();
    let best_audio = |ext: &str| -> Option<&Format> {
        usable
            .iter()
            .copied()
            .filter(|f| f.has_audio() && !f.has_video() && f.ext == ext)
            .max_by(|a, b| a.language_preference.cmp(&b.language_preference).then(a.bitrate.unwrap_or(0.0).total_cmp(&b.bitrate.unwrap_or(0.0))))
    };
    let (m4a, opus) = (best_audio("m4a"), best_audio("webm"));

    let mut heights: Vec<u32> = usable.iter().filter(|f| f.has_video()).filter_map(|f| f.height).collect();
    heights.sort_unstable_by(|a, b| b.cmp(a));
    heights.dedup();

    let mut out = Vec::new();
    for height in heights {
        // Per height: the codec that plays in the most places, then the
        // higher frame rate, then the higher bitrate.
        let Some(video) = usable.iter().copied().filter(|f| f.has_video() && f.height == Some(height)).min_by(|a, b| {
            a.codec_family()
                .cmp(&b.codec_family())
                .then(b.fps.unwrap_or(0.0).total_cmp(&a.fps.unwrap_or(0.0)))
                .then(b.bitrate.unwrap_or(0.0).total_cmp(&a.bitrate.unwrap_or(0.0)))
        }) else {
            continue;
        };
        let fps = video.fps.map(|f| f.round() as u32).filter(|f| *f > 30);
        let label = match fps {
            Some(fps) => format!("{height}p{fps}"),
            None => format!("{height}p"),
        };
        let (audio, container) = if video.has_audio() {
            (None, if video.ext == "webm" { "webm" } else { "mp4" })
        } else if video.ext == "webm" {
            // WebM takes Opus; MP4 sound in it would not play.
            match opus.or(m4a) {
                Some(a) if a.ext == "webm" => (Some(a), "webm"),
                Some(a) => (Some(a), "mkv"),
                None => (None, "webm"),
            }
        } else {
            match m4a.or(opus) {
                Some(a) if a.ext == "m4a" => (Some(a), "mp4"),
                Some(a) => (Some(a), "mkv"),
                None => (None, "mp4"),
            }
        };
        let size = match (video.size, audio.map(|a| a.size)) {
            (Some(v), Some(Some(a))) => Some(v + a),
            (Some(v), None) => Some(v),
            _ => None,
        };
        out.push(Variant {
            id: height.to_string(),
            label,
            detail: format!("{}  \u{00B7}  {}", container.to_ascii_uppercase(), video.codec_family().label()),
            size,
            video: Some(video.clone()),
            audio: audio.cloned(),
            container,
        });
    }
    if let Some(audio) = m4a.or(opus) {
        out.push(Variant {
            id: AUDIO_ONLY.to_owned(),
            label: "Sound only".to_owned(),
            detail: if audio.ext == "m4a" { "M4A  \u{00B7}  AAC".to_owned() } else { "WebM  \u{00B7}  Opus".to_owned() },
            size: audio.size,
            video: None,
            audio: Some(audio.clone()),
            container: if audio.ext == "m4a" { "m4a" } else { "webm" },
        });
    }
    out
}

/// Finds the choice to use: the one asked for, or else the best picture not
/// above `preferred_height`, or else the smallest picture there is.
pub fn pick<'a>(variants: &'a [Variant], wanted: Option<&str>, preferred_height: u32) -> Option<&'a Variant> {
    if let Some(wanted) = wanted
        && let Some(found) = variants.iter().find(|v| v.id == wanted)
    {
        return Some(found);
    }
    let pictures = || variants.iter().filter(|v| v.video.is_some());
    let height = |v: &Variant| v.id.parse::<u32>().unwrap_or(0);
    // A stored height that this page does not offer: the nearest one below it.
    let ceiling = wanted.and_then(|w| w.parse::<u32>().ok()).unwrap_or(preferred_height);
    pictures().filter(|v| height(v) <= ceiling).max_by_key(|v| height(v)).or_else(|| pictures().min_by_key(|v| height(v))).or_else(|| variants.first())
}

/// `"12:34"` or `"1:02:03"`.
pub fn clock(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

fn tool(name: &str) -> Option<PathBuf> {
    let name = if cfg!(windows) { format!("{name}.exe") } else { name.to_owned() };
    search_dirs().into_iter().map(|dir| dir.join(&name)).find(|path| path.is_file())
}

/// Where command-line tools are looked for. An application started from the
/// Dock does not inherit the shell's `PATH`, so the usual places are added.
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/opt/local/bin"].map(PathBuf::from));
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.extend([home.join(".local/bin"), home.join(".deno/bin"), home.join("bin")]);
    }
    dirs
}

/// Where `yt-dlp` lives, if it is installed.
pub fn find_resolver() -> Option<PathBuf> {
    tool("yt-dlp")
}

fn as_text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned).filter(|s| !s.is_empty() && s != "none")
}

fn parse_format(value: &Value) -> Option<Format> {
    // Only plain files: manifests and storyboards are other protocols.
    if value.get("protocol").and_then(Value::as_str) != Some("https") {
        return None;
    }
    let url = as_text(value, "url")?;
    let (vcodec, acodec) = (as_text(value, "vcodec"), as_text(value, "acodec"));
    if vcodec.is_none() && acodec.is_none() {
        return None;
    }
    let headers = value
        .get("http_headers")
        .and_then(Value::as_object)
        .map(|map| map.iter().filter_map(|(name, value)| value.as_str().map(|v| (name.clone(), v.to_owned()))).collect())
        .unwrap_or_default();
    Some(Format {
        id: as_text(value, "format_id")?,
        ext: as_text(value, "ext").unwrap_or_default(),
        vcodec,
        acodec,
        height: value.get("height").and_then(Value::as_u64).map(|h| h as u32),
        fps: value.get("fps").and_then(Value::as_f64),
        bitrate: value.get("tbr").and_then(Value::as_f64),
        size: value.get("filesize").and_then(Value::as_u64).or_else(|| value.get("filesize_approx").and_then(Value::as_u64)),
        url,
        headers,
        request_size: value.get("downloader_options").and_then(|o| o.get("http_chunk_size")).and_then(Value::as_u64),
        language_preference: value.get("language_preference").and_then(Value::as_i64).unwrap_or(0),
    })
}

/// Reads the resolver's description of a page.
pub fn parse(json: &[u8]) -> Result<Resolved> {
    let value: Value = serde_json::from_slice(json).map_err(|e| Error::Protocol(format!("the page description could not be read: {e}")))?;
    let formats: Vec<Format> = value.get("formats").and_then(Value::as_array).map(|list| list.iter().filter_map(parse_format).collect()).unwrap_or_default();
    Ok(Resolved {
        title: as_text(&value, "title").unwrap_or_else(|| "Video".to_owned()),
        duration: value.get("duration").and_then(Value::as_f64).map(|d| d.round() as u64),
        live: value.get("is_live").and_then(Value::as_bool).unwrap_or(false),
        formats,
    })
}

/// Asks the resolver what a page offers. Fetches the description only; no
/// media is downloaded by it.
pub async fn resolve(resolver: &Path, page_url: &str) -> Result<Resolved> {
    let page = url::Url::parse(page_url.trim()).map_err(|e| Error::InvalidUrl(format!("{page_url}: {e}")))?;
    if !matches!(page.scheme(), "http" | "https") {
        return Err(Error::InvalidUrl("only web pages can be resolved".into()));
    }
    let mut command = tokio::process::Command::new(resolver);
    command
        // The user's own configuration could change what is printed.
        .args(["--ignore-config", "--no-playlist", "--no-warnings", "--no-progress", "--socket-timeout", "20", "-J", "--"])
        .arg(page.as_str())
        // The resolver needs to find its own helpers (a JavaScript runtime).
        .env("PATH", std::env::join_paths(search_dirs()).unwrap_or_default())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(RESOLVE_TIMEOUT, command.output())
        .await
        .map_err(|_| Error::Timeout)?
        .map_err(|e| Error::Unsupported(format!("the video helper could not be started: {e}")))?;
    if !output.status.success() {
        // Its last "ERROR:" line is written for people.
        let text = String::from_utf8_lossy(&output.stderr);
        let reason = text.lines().rev().find_map(|line| line.trim().strip_prefix("ERROR:")).map(|line| {
            // "[youtube] abc123: Video unavailable" reads better without its prefix.
            let line = line.trim();
            line.split_once("] ").map(|(_, rest)| rest).and_then(|rest| rest.split_once(": ").map(|(_, message)| message)).unwrap_or(line).to_owned()
        });
        return Err(Error::Protocol(reason.unwrap_or_else(|| "the page could not be read".to_owned())));
    }
    let resolved = parse(&output.stdout)?;
    if resolved.live {
        return Err(Error::Unsupported("this is a live broadcast; it can be saved once it has ended".into()));
    }
    Ok(resolved)
}

/// Joins a picture stream and a sound stream into one file without
/// re-encoding either.
pub async fn join(ffmpeg: &Path, video: &Path, audio: &Path, output: &Path) -> Result<()> {
    // Written under a temporary name: a half-written file must never carry
    // the final one.
    let mut scratch = output.as_os_str().to_owned();
    scratch.push(".joining");
    let scratch = PathBuf::from(scratch);
    let _ = std::fs::remove_file(&scratch);
    let format = match output.extension().and_then(|e| e.to_str()) {
        Some("webm") => "webm",
        Some("mkv") => "matroska",
        _ => "mp4",
    };
    let mut command = tokio::process::Command::new(ffmpeg);
    command.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"]).arg(video).arg("-i").arg(audio).args(["-map", "0:v:0", "-map", "1:a:0", "-c", "copy"]);
    if format == "mp4" {
        // The index goes to the front, so the file starts playing at once.
        command.args(["-movflags", "+faststart"]);
    }
    command.args(["-f", format]).arg(&scratch).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
    let output_of = command.output().await.map_err(|e| Error::Unsupported(format!("ffmpeg could not be started: {e}")))?;
    if !output_of.status.success() {
        let _ = std::fs::remove_file(&scratch);
        let text = String::from_utf8_lossy(&output_of.stderr);
        return Err(Error::Protocol(format!("joining picture and sound failed: {}", text.lines().last().unwrap_or("ffmpeg reported an error").trim())));
    }
    std::fs::rename(&scratch, output).map_err(|e| Error::Disk(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn format(id: &str, ext: &str, vcodec: Option<&str>, acodec: Option<&str>, height: Option<u32>, fps: f64, bitrate: f64, size: u64) -> Value {
        serde_json::json!({
            "format_id": id, "ext": ext, "protocol": "https",
            "vcodec": vcodec.unwrap_or("none"), "acodec": acodec.unwrap_or("none"),
            "height": height, "fps": fps, "tbr": bitrate, "filesize": size,
            "url": format!("https://media.example/stream?id={id}"),
            "http_headers": { "User-Agent": "Resolver/1.0" },
            "downloader_options": { "http_chunk_size": 10485760 },
            "language_preference": 10,
        })
    }

    fn sample() -> Resolved {
        let page = serde_json::json!({
            "title": "A walk in the hills", "duration": 635.2, "is_live": false,
            "formats": [
                { "format_id": "sb0", "protocol": "mhtml", "url": "https://media.example/storyboard", "vcodec": "none", "acodec": "none" },
                { "format_id": "hls-1", "protocol": "m3u8_native", "url": "https://media.example/a.m3u8", "vcodec": "avc1", "acodec": "mp4a", "height": 720 },
                format("140-drc", "m4a", None, Some("mp4a.40.2"), None, 0.0, 129.5, 10_000_000),
                format("139", "m4a", None, Some("mp4a.40.5"), None, 0.0, 48.8, 3_800_000),
                format("140", "m4a", None, Some("mp4a.40.2"), None, 0.0, 129.4, 10_200_000),
                format("251", "webm", None, Some("opus"), None, 0.0, 128.6, 10_100_000),
                format("18", "mp4", Some("avc1.42001E"), Some("mp4a.40.2"), Some(360), 30.0, 500.0, 40_000_000),
                format("134", "mp4", Some("avc1.4D401E"), None, Some(360), 30.0, 230.0, 18_000_000),
                format("298", "mp4", Some("avc1.4D4020"), None, Some(720), 60.0, 1897.0, 150_000_000),
                format("302", "webm", Some("vp9"), None, Some(720), 60.0, 1420.0, 112_000_000),
                format("299", "mp4", Some("avc1.64002A"), None, Some(1080), 60.0, 3247.0, 257_000_000),
                format("399", "mp4", Some("av01.0.09M.08"), None, Some(1080), 60.0, 1568.0, 124_000_000),
                format("308", "webm", Some("vp9"), None, Some(1440), 60.0, 5967.0, 473_000_000),
                format("400", "mp4", Some("av01.0.12M.08"), None, Some(1440), 60.0, 3940.0, 312_000_000),
                format("315", "webm", Some("vp9"), None, Some(2160), 60.0, 17174.0, 1_362_000_000),
            ],
        });
        parse(page.to_string().as_bytes()).unwrap()
    }

    #[test]
    fn the_description_is_read_and_only_plain_files_are_kept() {
        let resolved = sample();
        assert_eq!(resolved.title, "A walk in the hills");
        assert_eq!(resolved.duration, Some(635));
        assert!(!resolved.live);
        assert_eq!(resolved.formats.len(), 13, "the storyboard and the manifest are not files");
        let audio = resolved.formats.iter().find(|f| f.id == "140").unwrap();
        assert_eq!((audio.vcodec.as_deref(), audio.acodec.as_deref()), (None, Some("mp4a.40.2")));
        assert_eq!(audio.request_size, Some(10 * 1024 * 1024));
        assert_eq!(audio.headers, vec![("User-Agent".to_owned(), "Resolver/1.0".to_owned())]);
        assert!(parse(b"not json").is_err());
        assert!(parse(b"{}").unwrap().formats.is_empty());
    }

    #[test]
    fn one_choice_per_height_with_the_codec_that_plays_most_widely() {
        let resolved = sample();
        let choices = variants(&resolved);
        let ids: Vec<&str> = choices.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(ids, vec!["2160", "1440", "1080", "720", "360", AUDIO_ONLY]);

        let full_hd = &choices[2];
        assert_eq!(full_hd.label, "1080p60");
        assert_eq!(full_hd.video.as_ref().unwrap().id, "299", "H.264 is preferred where it exists");
        assert_eq!(full_hd.audio.as_ref().unwrap().id, "140", "the best AAC track, not its compressed-dynamics twin");
        assert_eq!(full_hd.container, "mp4");
        assert_eq!(full_hd.size, Some(267_200_000));
        assert!(full_hd.needs_joining());
        assert_eq!(full_hd.stages().iter().map(|(name, f)| (*name, f.id.as_str())).collect::<Vec<_>>(), vec![("video", "299"), ("audio", "140")]);

        // No H.264 above 1080p: AV1 in MP4 before VP9 in WebM.
        assert_eq!((choices[1].video.as_ref().unwrap().id.as_str(), choices[1].container), ("400", "mp4"));
        // Only VP9 at 2160p: WebM, with the Opus track.
        assert_eq!((choices[0].video.as_ref().unwrap().id.as_str(), choices[0].audio.as_ref().unwrap().id.as_str(), choices[0].container), ("315", "251", "webm"));
        // 30 frames a second is the unremarkable case and is not spelled out.
        assert_eq!(choices[4].label, "360p");
        // At 360p the separate picture stream ranks above the muxed one by bitrate order only if better; either way it is one choice.
        assert_eq!(choices.iter().filter(|v| v.id == "360").count(), 1);

        let sound = choices.last().unwrap();
        assert_eq!((sound.audio.as_ref().unwrap().id.as_str(), sound.container, sound.video.is_none()), ("140", "m4a", true));
        assert!(!sound.needs_joining());
    }

    #[test]
    fn picking_a_choice() {
        let resolved = sample();
        let choices = variants(&resolved);
        let id = |wanted: Option<&str>, preferred: u32| pick(&choices, wanted, preferred).unwrap().id.clone();
        assert_eq!(id(None, 1080), "1080");
        assert_eq!(id(None, 1000), "720", "the best picture not above the preference");
        assert_eq!(id(None, 100), "360", "nothing that small: the smallest there is");
        assert_eq!(id(None, 9999), "2160");
        assert_eq!(id(Some("720"), 1080), "720");
        assert_eq!(id(Some(AUDIO_ONLY), 1080), AUDIO_ONLY);
        // A stored choice this page does not have: the nearest below it.
        assert_eq!(id(Some("480"), 2160), "360");
        assert!(pick(&[], None, 1080).is_none());
    }

    #[test]
    fn pages_are_told_from_files() {
        for page in [
            "https://www.youtube.com/watch?v=aqz-KE-bpKQ",
            "https://youtube.com/watch?v=aqz-KE-bpKQ&list=PL123&index=4&t=30s",
            "https://m.youtube.com/watch?v=aqz-KE-bpKQ",
            "https://music.youtube.com/watch?v=aqz-KE-bpKQ",
            "https://youtu.be/aqz-KE-bpKQ?si=tracking",
            "https://www.youtube.com/shorts/abcdefghijk",
            "https://www.youtube.com/embed/aqz-KE-bpKQ",
        ] {
            assert!(is_page(page), "{page}");
        }
        for other in [
            "https://www.youtube.com/",
            "https://www.youtube.com/watch",
            "https://www.youtube.com/feed/subscriptions",
            "https://youtu.be/",
            "https://notyoutube.com/watch?v=x",
            "https://example.com/watch?v=aqz-KE-bpKQ",
            "https://rr3---sn-abc.googlevideo.com/videoplayback?itag=140",
            "ftp://youtube.com/watch?v=x",
            "not an address",
        ] {
            assert!(!is_page(other), "{other}");
        }
        assert_eq!(canonical("https://www.youtube.com/watch?v=aqz-KE-bpKQ&list=PL123&index=4&t=30s#top"), "https://www.youtube.com/watch?v=aqz-KE-bpKQ");
        assert_eq!(canonical("https://youtu.be/aqz-KE-bpKQ?si=tracking"), "https://youtu.be/aqz-KE-bpKQ");
        assert_eq!(clock(635), "10:35");
        assert_eq!(clock(3723), "1:02:03");
        assert_eq!(clock(5), "0:05");
    }
}
