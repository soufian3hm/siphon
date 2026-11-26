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

/// A host name without the prefixes sites put in front of the same pages.
fn bare_host(host: &str) -> &str {
    let mut host = host;
    for prefix in ["www.", "m.", "mobile.", "web.", "music.", "old.", "new.", "touch.", "mbasic."] {
        host = host.strip_prefix(prefix).unwrap_or(host);
    }
    host
}

fn is_youtube(host: &str) -> bool {
    let host = bare_host(host);
    host == "youtube.com" || host == "youtube-nocookie.com" || host == "youtu.be"
}

fn digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// Whether an address is, by its shape alone, the page of one video on a
/// site the helper knows. Addresses that could be anything (a post that may
/// or may not carry a video) are not claimed here: the browser extension,
/// which has seen the player, says so itself.
pub fn is_page(url: &str) -> bool {
    let Ok(url) = url::Url::parse(url.trim()) else { return false };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let path = url.path();
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let has = |key: &str| url.query_pairs().any(|(name, value)| name == key && !value.is_empty());
    match bare_host(&host) {
        "youtu.be" => !parts.is_empty(),
        "youtube.com" | "youtube-nocookie.com" => {
            (path == "/watch" && has("v")) || ["/shorts/", "/embed/", "/live/", "/v/"].iter().any(|prefix| path.starts_with(prefix) && path.len() > prefix.len())
        }
        "x.com" | "twitter.com" => parts.windows(2).any(|pair| pair[0] == "status" && digits(pair[1])),
        "facebook.com" => {
            (parts.first() == Some(&"watch") && has("v"))
                || (parts.first() == Some(&"video.php") && has("v"))
                || parts.contains(&"videos")
                || (parts.len() >= 2 && parts[0] == "reel")
                || (parts.len() >= 3 && parts[0] == "share" && matches!(parts[1], "v" | "r"))
        }
        "fb.watch" => !parts.is_empty(),
        "instagram.com" => parts.len() >= 2 && matches!(parts[parts.len() - 2], "reel" | "reels" | "tv"),
        "tiktok.com" => parts.windows(2).any(|pair| pair[0] == "video" && digits(pair[1])),
        "vm.tiktok.com" | "vt.tiktok.com" => !parts.is_empty(),
        "vimeo.com" => parts.last().is_some_and(|last| digits(last)),
        "player.vimeo.com" => parts.len() >= 2 && parts[0] == "video" && digits(parts[1]),
        "dailymotion.com" => parts.len() >= 2 && parts[0] == "video",
        "dai.ly" => !parts.is_empty(),
        "twitch.tv" => (parts.len() >= 2 && parts[0] == "videos" && digits(parts[1])) || (parts.len() >= 3 && parts[1] == "clip"),
        "clips.twitch.tv" | "v.redd.it" | "streamable.com" => parts.len() == 1,
        _ => false,
    }
}

/// Query parameters that say how someone got to a page, not which page.
fn is_tracking(name: &str) -> bool {
    const NAMES: [&str; 30] = [
        "si", "feature", "t", "start", "list", "index", "pp", "ab_channel", "s", "ref", "ref_src", "ref_url", "src", "fbclid", "gclid", "igsh",
        "igshid", "img_index", "mibextid", "rdid", "share_url", "sfnsn", "extid", "locale", "context", "share_id", "notif_id", "notif_t", "comment_id", "reply_comment_id",
    ];
    name.starts_with("utm_") || name.starts_with("__") || NAMES.contains(&name)
}

/// The page address with everything but the video itself removed (playlist
/// position, start time, tracking), so the same video is always the same
/// download.
pub fn canonical(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url.trim()) else { return url.trim().to_owned() };
    let mut bare = parsed.clone();
    bare.set_fragment(None);
    let kept: Vec<(String, String)> = parsed.query_pairs().filter(|(name, _)| !is_tracking(name)).map(|(name, value)| (name.into_owned(), value.into_owned())).collect();
    if kept.is_empty() {
        bare.set_query(None);
    } else {
        bare.query_pairs_mut().clear().extend_pairs(kept);
    }
    bare.to_string()
}

/// How a stream is delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// One file, fetched in ranges.
    File,
    /// An HLS playlist of segments.
    Hls,
}

/// One stream as the resolver describes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Format {
    pub id: String,
    pub ext: String,
    /// `None` when the stream has no picture. An empty name is a picture
    /// whose codec the site does not tell.
    pub vcodec: Option<String>,
    /// `None` when the stream has no sound.
    pub acodec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    /// Total bitrate in kbit/s.
    pub bitrate: Option<f64>,
    pub size: Option<u64>,
    /// The size is worked out from bitrate and length, not stated.
    pub size_estimated: bool,
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// The largest range to ask for in one request.
    pub request_size: Option<u64>,
    /// Higher is closer to the original sound track.
    pub language_preference: i64,
    pub delivery: Delivery,
    /// For an HLS rendition: the master playlist that lists it.
    pub manifest: Option<String>,
    /// The site's own name for the quality (`"hd"`), where it has no height.
    pub note: Option<String>,
    /// The resolver's ranking among streams that say nothing else.
    pub rank: i64,
}

impl Format {
    fn has_video(&self) -> bool {
        self.vcodec.is_some()
    }

    fn has_audio(&self) -> bool {
        self.acodec.is_some()
    }

    /// The side by which a picture's quality is named: 1080 for 1920×1080
    /// and for an upright 1080×1920 alike.
    fn quality(&self) -> Option<u32> {
        match (self.width, self.height) {
            (Some(width), Some(height)) => Some(width.min(height)),
            (None, height) => height,
            (Some(_), None) => None,
        }
    }

    fn codec_family(&self) -> Codec {
        match self.vcodec.as_deref() {
            Some(c) if c.starts_with("avc1") || c.starts_with("h264") => Codec::H264,
            Some(c) if c.starts_with("av01") => Codec::Av1,
            Some(c) if c.starts_with("vp9") || c.starts_with("vp09") => Codec::Vp9,
            Some(c) if c.starts_with("hev1") || c.starts_with("hvc1") => Codec::Hevc,
            _ => Codec::Other,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Codec {
    // Ordered by how widely the result plays: best first.
    H264,
    Hevc,
    Av1,
    Vp9,
    Other,
}

impl Codec {
    fn label(self) -> Option<&'static str> {
        match self {
            Codec::H264 => Some("H.264"),
            Codec::Hevc => Some("HEVC"),
            Codec::Av1 => Some("AV1"),
            Codec::Vp9 => Some("VP9"),
            Codec::Other => None,
        }
    }
}

/// What a page offers.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolved {
    pub title: String,
    /// Who posted it, as the site shows the name.
    pub uploader: Option<String>,
    pub duration: Option<u64>,
    pub live: bool,
    pub formats: Vec<Format>,
    /// The resolver's name for the site (`"youtube"`, `"twitter"`).
    pub site: String,
}

impl Resolved {
    /// What to call the file: the video's own title where it has one, and
    /// on sites where posts have no title, who posted it and what they wrote.
    pub fn name(&self) -> String {
        // Links in a post's text say nothing about the video.
        let words: Vec<&str> = self.title.split_whitespace().filter(|word| !word.starts_with("http://") && !word.starts_with("https://")).collect();
        let mut title = words.join(" ");
        let uploader = self.uploader.as_deref().map(str::trim).filter(|name| !name.is_empty());
        let titled_sites = ["youtube", "vimeo", "dailymotion", "twitch"];
        if let Some(uploader) = uploader
            && !titled_sites.iter().any(|site| self.site.starts_with(site))
            && !title.to_lowercase().contains(&uploader.to_lowercase())
        {
            title = if title.is_empty() { uploader.to_owned() } else { format!("{uploader} - {title}") };
        }
        if title.is_empty() {
            title = "Video".to_owned();
        }
        // A post can run to paragraphs; a file name should not.
        const LIMIT: usize = 96;
        if title.chars().count() > LIMIT {
            let cut: String = title.chars().take(LIMIT).collect();
            let at_word = cut.rfind(' ').filter(|at| *at > LIMIT / 2).unwrap_or(cut.len());
            title = format!("{}\u{2026}", cut[..at_word].trim_end_matches([' ', ',', '.', ':', ';', '-']));
        }
        title
    }
}

/// One choice the user can make: a picture quality, or sound only.
#[derive(Clone, Debug, PartialEq)]
pub struct Variant {
    /// The picture's quality as text (`"1080"`), the site's own name for it
    /// where it has no size (`"hd"`), or [`AUDIO_ONLY`]. Stable across
    /// resolutions of the same page, so a stored choice can be found again.
    pub id: String,
    /// `"1080p60"`.
    pub label: String,
    /// `"MP4 · H.264"`.
    pub detail: String,
    pub size: Option<u64>,
    /// The size is an estimate.
    pub size_estimated: bool,
    /// The quality as a number, 0 where the site gives none.
    pub height: u32,
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
        !self.is_hls() && self.video.as_ref().is_some_and(|v| !v.has_audio()) && self.audio.is_some()
    }

    /// The picture comes as an HLS stream, which the stream engine fetches
    /// and assembles whole, sound included.
    pub fn is_hls(&self) -> bool {
        self.video.as_ref().is_some_and(|video| video.delivery == Delivery::Hls)
    }
}

fn sum_sizes(video: &Format, audio: Option<&Format>) -> (Option<u64>, bool) {
    match (video.size, audio) {
        (Some(v), Some(a)) => (a.size.map(|a| v + a), video.size_estimated || a.size_estimated),
        (Some(v), None) => (Some(v), video.size_estimated),
        (None, _) => (None, false),
    }
}

/// The quality choices a page offers, best picture first, sound only last.
pub fn variants(resolved: &Resolved) -> Vec<Variant> {
    // Compressed-dynamics duplicates of the sound are left out.
    let usable: Vec<&Format> = resolved.formats.iter().filter(|f| !f.url.is_empty() && !f.id.contains("drc")).collect();
    let files: Vec<&Format> = usable.iter().copied().filter(|f| f.delivery == Delivery::File).collect();
    let mp4_sound = |f: &Format| matches!(f.ext.as_str(), "m4a" | "mp4" | "aac" | "mp3");
    let best_audio = |mp4: bool| -> Option<&Format> {
        files
            .iter()
            .copied()
            .filter(|f| f.has_audio() && !f.has_video() && mp4_sound(f) == mp4)
            .max_by(|a, b| a.language_preference.cmp(&b.language_preference).then(a.bitrate.unwrap_or(0.0).total_cmp(&b.bitrate.unwrap_or(0.0))))
    };
    let (m4a, opus) = (best_audio(true), best_audio(false));

    // Per quality: the codec that plays in the most places, then the higher
    // frame rate, then the higher bitrate.
    let best_of = |candidates: &[&Format], quality: u32| -> Option<Format> {
        candidates
            .iter()
            .copied()
            .filter(|f| f.has_video() && f.quality() == Some(quality))
            .min_by(|a, b| {
                a.codec_family()
                    .cmp(&b.codec_family())
                    .then(b.fps.unwrap_or(0.0).total_cmp(&a.fps.unwrap_or(0.0)))
                    .then(b.bitrate.unwrap_or(0.0).total_cmp(&a.bitrate.unwrap_or(0.0)))
            })
            .cloned()
    };
    let qualities = |candidates: &[&Format]| -> Vec<u32> {
        let mut found: Vec<u32> = candidates.iter().filter(|f| f.has_video()).filter_map(|f| f.quality()).filter(|q| *q > 0).collect();
        found.sort_unstable_by(|a, b| b.cmp(a));
        found.dedup();
        found
    };
    let label_of = |video: &Format, quality: u32| match video.fps.map(|f| f.round() as u32).filter(|f| *f > 30) {
        Some(fps) => format!("{quality}p{fps}"),
        None => format!("{quality}p"),
    };
    let detail_of = |container: &str, video: &Format| match video.codec_family().label() {
        Some(codec) => format!("{}  \u{00B7}  {codec}", container.to_ascii_uppercase()),
        None => container.to_ascii_uppercase(),
    };
    // The sound that goes with a picture, and the container that holds both.
    let pair = |video: &Format| -> (Option<&Format>, &'static str) {
        if video.has_audio() {
            (None, if video.ext == "webm" { "webm" } else { "mp4" })
        } else if video.ext == "webm" {
            // WebM takes Opus; MP4 sound in it would not play.
            match opus.or(m4a) {
                Some(a) if !mp4_sound(a) => (Some(a), "webm"),
                Some(a) => (Some(a), "mkv"),
                None => (None, "webm"),
            }
        } else {
            match m4a.or(opus) {
                Some(a) if mp4_sound(a) => (Some(a), "mp4"),
                Some(a) => (Some(a), "mkv"),
                None => (None, "mp4"),
            }
        }
    };

    let mut out = Vec::new();
    for quality in qualities(&files) {
        let Some(video) = best_of(&files, quality) else { continue };
        let (audio, container) = pair(&video);
        let (size, size_estimated) = sum_sizes(&video, audio);
        out.push(Variant {
            id: quality.to_string(),
            label: label_of(&video, quality),
            detail: detail_of(container, &video),
            size,
            size_estimated,
            height: quality,
            audio: audio.cloned(),
            video: Some(video),
            container,
        });
    }
    // Files the site names but does not measure ("hd", "sd"), best first.
    let mut unnamed: Vec<&Format> = files.iter().copied().filter(|f| f.has_video() && f.quality().is_none_or(|q| q == 0)).collect();
    unnamed.sort_by(|a, b| b.rank.cmp(&a.rank).then(b.bitrate.unwrap_or(0.0).total_cmp(&a.bitrate.unwrap_or(0.0))));
    for video in unnamed {
        let (audio, container) = pair(video);
        let (size, size_estimated) = sum_sizes(video, audio);
        let name = video.note.clone().unwrap_or_else(|| video.id.clone());
        out.push(Variant {
            id: video.id.clone(),
            label: if name.len() <= 4 { name.to_ascii_uppercase() } else { name },
            detail: detail_of(container, video),
            size,
            size_estimated,
            height: 0,
            video: Some(video.clone()),
            audio: audio.cloned(),
            container,
        });
    }
    // No files at all: the site only streams. One choice per quality of the
    // stream, fetched by the stream engine.
    if out.is_empty() {
        let streams: Vec<&Format> = usable.iter().copied().filter(|f| f.delivery == Delivery::Hls).collect();
        for quality in qualities(&streams) {
            let Some(video) = best_of(&streams, quality) else { continue };
            out.push(Variant {
                id: quality.to_string(),
                label: label_of(&video, quality),
                detail: match video.codec_family().label() {
                    Some(codec) => format!("MP4  \u{00B7}  {codec}  \u{00B7}  stream"),
                    None => "MP4  \u{00B7}  stream".to_owned(),
                },
                size: video.size,
                size_estimated: video.size_estimated,
                height: quality,
                video: Some(video),
                audio: None,
                container: "mp4",
            });
        }
    }
    if let Some(audio) = m4a.or(opus) {
        let mp4 = mp4_sound(audio);
        out.push(Variant {
            id: AUDIO_ONLY.to_owned(),
            label: "Sound only".to_owned(),
            detail: if mp4 { "M4A  \u{00B7}  AAC".to_owned() } else { "WebM  \u{00B7}  Opus".to_owned() },
            size: audio.size,
            size_estimated: audio.size_estimated,
            height: 0,
            video: None,
            audio: Some(audio.clone()),
            container: if !mp4 {
                "webm"
            } else if audio.ext == "mp3" {
                "mp3"
            } else {
                "m4a"
            },
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
    let pictures = || variants.iter().filter(|v| v.video.is_some() && v.height > 0);
    // A stored quality that this page does not offer: the nearest one below it.
    let ceiling = wanted.and_then(|w| w.parse::<u32>().ok()).unwrap_or(preferred_height);
    pictures()
        .filter(|v| v.height <= ceiling)
        .max_by_key(|v| v.height)
        .or_else(|| pictures().min_by_key(|v| v.height))
        // Pictures without a stated size are listed best first.
        .or_else(|| variants.iter().find(|v| v.video.is_some()))
        .or_else(|| variants.first())
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
    // `SIPHON_RESOLVER` names another copy, or a stand-in for tests.
    std::env::var_os("SIPHON_RESOLVER").map(PathBuf::from).filter(|path| path.is_file()).or_else(|| tool("yt-dlp"))
}

fn as_text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned).filter(|s| !s.is_empty() && s != "none")
}

/// A codec field: `"none"` says the stream lacks that part; a missing field
/// says only that the site did not tell, and the part is taken to be there.
fn codec(value: &Value, key: &str) -> Option<String> {
    match value.get(key).and_then(Value::as_str) {
        Some("none") => None,
        Some(name) => Some(name.to_owned()),
        None => Some(String::new()),
    }
}

/// The request cookies a stream needs, from the resolver's cookie list
/// (`name=value; Domain=…; Path=…; name2=value2; …`).
fn cookie_header(list: &str) -> Option<String> {
    const ATTRIBUTES: [&str; 8] = ["domain", "path", "expires", "version", "max-age", "secure", "httponly", "samesite"];
    let pairs: Vec<&str> = list
        .split(';')
        .map(str::trim)
        .filter(|part| part.split_once('=').is_some_and(|(name, _)| !ATTRIBUTES.contains(&name.trim().to_ascii_lowercase().as_str())))
        .collect();
    (!pairs.is_empty()).then(|| pairs.join("; "))
}

fn parse_format(value: &Value, duration: Option<f64>) -> Option<Format> {
    // Plain files and HLS streams. Storyboards, DASH segment lists and the
    // rest are other protocols.
    let delivery = match value.get("protocol").and_then(Value::as_str) {
        Some("https" | "http") => Delivery::File,
        Some("m3u8_native" | "m3u8") => Delivery::Hls,
        _ => return None,
    };
    let url = as_text(value, "url")?;
    let sound_only = value.get("resolution").and_then(Value::as_str) == Some("audio only");
    let vcodec = if sound_only { None } else { codec(value, "vcodec") };
    let acodec = codec(value, "acodec");
    if vcodec.is_none() && acodec.is_none() {
        return None;
    }
    let mut headers: Vec<(String, String)> = value
        .get("http_headers")
        .and_then(Value::as_object)
        .map(|map| map.iter().filter_map(|(name, value)| value.as_str().map(|v| (name.clone(), v.to_owned()))).collect())
        .unwrap_or_default();
    if let Some(cookies) = value.get("cookies").and_then(Value::as_str).and_then(cookie_header) {
        headers.push(("Cookie".to_owned(), cookies));
    }
    let bitrate = value.get("tbr").and_then(Value::as_f64).filter(|rate| *rate > 0.0);
    let stated = value.get("filesize").and_then(Value::as_u64);
    let estimated = value.get("filesize_approx").and_then(Value::as_u64).or_else(|| match (bitrate, duration) {
        (Some(rate), Some(length)) if length > 0.0 => Some((rate * 1000.0 / 8.0 * length) as u64),
        _ => None,
    });
    let side = |key: &str| value.get(key).and_then(Value::as_u64).map(|side| side as u32).filter(|side| *side > 0);
    Some(Format {
        id: as_text(value, "format_id")?,
        ext: as_text(value, "ext").unwrap_or_default(),
        vcodec,
        acodec,
        width: side("width"),
        height: side("height"),
        fps: value.get("fps").and_then(Value::as_f64),
        bitrate,
        size: stated.or(estimated),
        size_estimated: stated.is_none() && estimated.is_some(),
        url,
        headers,
        request_size: value.get("downloader_options").and_then(|o| o.get("http_chunk_size")).and_then(Value::as_u64),
        language_preference: value.get("language_preference").and_then(Value::as_i64).unwrap_or(0),
        delivery,
        manifest: as_text(value, "manifest_url"),
        note: as_text(value, "format_note").filter(|note| note.len() <= 12),
        rank: value.get("quality").and_then(Value::as_f64).map(|rank| rank as i64).unwrap_or(0),
    })
}

/// Reads the resolver's description of a page.
pub fn parse(json: &[u8]) -> Result<Resolved> {
    let outer: Value = serde_json::from_slice(json).map_err(|e| Error::Protocol(format!("the page description could not be read: {e}")))?;
    // A post with several videos is described as a list; the address says
    // which one is meant, and without that it is the first.
    let value = outer.get("entries").and_then(Value::as_array).and_then(|entries| entries.iter().find(|entry| entry.get("formats").is_some())).unwrap_or(&outer);
    let text = |key: &str| as_text(value, key).or_else(|| as_text(&outer, key));
    let duration = value.get("duration").and_then(Value::as_f64);
    let formats: Vec<Format> = value.get("formats").and_then(Value::as_array).map(|list| list.iter().filter_map(|format| parse_format(format, duration)).collect()).unwrap_or_default();
    Ok(Resolved {
        title: text("title").unwrap_or_else(|| "Video".to_owned()),
        uploader: text("uploader").or_else(|| text("channel")).or_else(|| text("uploader_id")),
        duration: duration.map(|d| d.round() as u64),
        live: value.get("is_live").and_then(Value::as_bool).unwrap_or(false),
        formats,
        site: text("extractor_key").or_else(|| text("extractor")).unwrap_or_default().to_ascii_lowercase(),
    })
}

/// What the browser knew when it handed a page over.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Session {
    /// The page the player was embedded in, when that is another site.
    pub referer: Option<String>,
    /// The browser's cookies for the page, as a `Cookie` header value.
    pub cookies: Option<String>,
}

impl Session {
    pub fn of(request: &crate::types::Request) -> Self {
        Self { referer: request.referer.clone(), cookies: request.header("cookie").map(str::to_owned).filter(|c| !c.trim().is_empty()) }
    }
}

/// The domains a cookie of `host` is written for: the host and each parent
/// down to two labels, so a login on `www.site.com` also reaches
/// `api.site.com`.
fn cookie_domains(host: &str) -> Vec<String> {
    let labels: Vec<&str> = host.split('.').filter(|label| !label.is_empty()).collect();
    // A bare name or a numeric address has no parent to share a login with.
    if labels.len() < 2 || host.parse::<std::net::IpAddr>().is_ok() {
        return vec![host.to_owned()];
    }
    (0..=labels.len() - 2).map(|from| format!(".{}", labels[from..].join("."))).collect()
}

/// A cookie file in the format the resolver reads, deleted when dropped.
struct CookieFile(PathBuf);

impl CookieFile {
    fn write(host: &str, header: &str) -> std::io::Result<Self> {
        use std::io::Write;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("siphon-session-{}-{unique}.txt", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            // The file holds a login: nobody else may read it.
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        let guard = Self(path);
        writeln!(file, "# Netscape HTTP Cookie File")?;
        for domain in cookie_domains(host) {
            for pair in header.split(';') {
                let Some((name, value)) = pair.trim().split_once('=') else { continue };
                // A tab or a line break would break the file's columns.
                if name.is_empty() || name.contains(['\t', '\n', '\r']) || value.contains(['\t', '\n', '\r']) {
                    continue;
                }
                writeln!(file, "{domain}\tTRUE\t/\tTRUE\t2147483647\t{name}\t{value}")?;
            }
        }
        file.flush()?;
        Ok(guard)
    }
}

impl Drop for CookieFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Whether a page may be read with the browser's login. Never YouTube: an
/// account used that way can be restricted by the site, and its public
/// videos do not need it.
pub fn may_use_login(page_url: &str) -> bool {
    url::Url::parse(page_url.trim()).ok().and_then(|url| url.host_str().map(|host| !is_youtube(&host.to_ascii_lowercase()))).unwrap_or(false)
}

/// Asks the resolver what a page offers. Fetches the description only; no
/// media is downloaded by it. `with_login` hands it the session's cookies.
pub async fn resolve(resolver: &Path, page_url: &str, session: &Session, with_login: bool) -> Result<Resolved> {
    let page = url::Url::parse(page_url.trim()).map_err(|e| Error::InvalidUrl(format!("{page_url}: {e}")))?;
    if !matches!(page.scheme(), "http" | "https") {
        return Err(Error::InvalidUrl("only web pages can be resolved".into()));
    }
    let host = page.host_str().unwrap_or("").to_ascii_lowercase();
    let mut command = tokio::process::Command::new(resolver);
    // The user's own configuration could change what is printed.
    command.args(["--ignore-config", "--no-playlist", "--no-warnings", "--no-progress", "--socket-timeout", "20"]);
    // A player embedded in another site's page may only answer to that page.
    if let Some(referer) = session.referer.as_deref().filter(|referer| url::Url::parse(referer).ok().and_then(|r| r.host_str().map(|h| h.to_ascii_lowercase())).is_some_and(|h| h != host)) {
        command.arg("--add-headers").arg(format!("Referer:{referer}"));
    }
    let cookies = match (with_login, session.cookies.as_deref()) {
        (true, Some(header)) => Some(CookieFile::write(&host, header).map_err(|e| Error::Disk(e.to_string()))?),
        _ => None,
    };
    if let Some(file) = &cookies {
        command.arg("--cookies").arg(&file.0);
    }
    command
        .args(["-J", "--"])
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
    drop(cookies);
    if !output.status.success() {
        // Its last "ERROR:" line is written for people.
        let text = String::from_utf8_lossy(&output.stderr);
        let reason = text.lines().rev().find_map(|line| line.trim().strip_prefix("ERROR:")).map(|line| {
            // "[youtube] abc123: Video unavailable" reads better without its prefix.
            let line = line.trim();
            let message = line.split_once("] ").map(|(_, rest)| rest).and_then(|rest| rest.split_once(": ").map(|(_, message)| message)).unwrap_or(line);
            // Advice about the resolver's own command line means nothing here.
            let message = message.split(" Use --cookies").next().unwrap_or(message).split(" See  https://").next().unwrap_or(message);
            message.trim().trim_end_matches('.').to_owned()
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
        assert_eq!(resolved.formats.len(), 14, "the storyboard is not a stream");
        assert_eq!(resolved.formats.iter().filter(|f| f.delivery == Delivery::Hls).count(), 1);
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
        // What names the video stays; how the visitor got there goes.
        assert_eq!(canonical("https://www.facebook.com/watch/?v=10155529876156509&ref=sharing&__tn__=R#x"), "https://www.facebook.com/watch/?v=10155529876156509");
        assert_eq!(canonical("https://x.com/historyinmemes/status/1790637656616943991?s=20&t=abc"), "https://x.com/historyinmemes/status/1790637656616943991");
        assert_eq!(canonical("https://www.reddit.com/r/aww/comments/90bu6w/a_title/?utm_source=share&utm_medium=web"), "https://www.reddit.com/r/aww/comments/90bu6w/a_title/");
        assert_eq!(clock(635), "10:35");
        assert_eq!(clock(3723), "1:02:03");
        assert_eq!(clock(5), "0:05");
    }

    #[test]
    fn the_pages_of_other_sites_are_known_by_their_shape() {
        for page in [
            "https://x.com/historyinmemes/status/1790637656616943991",
            "https://twitter.com/CTVJLaidlaw/status/1600649710662213632/video/2",
            "https://mobile.twitter.com/i/web/status/910031516746514432",
            "https://www.facebook.com/cnn/videos/10155529876156509/",
            "https://www.facebook.com/watch/?v=10155529876156509",
            "https://m.facebook.com/video.php?v=274175099429670",
            "https://www.facebook.com/reel/1234567890",
            "https://www.facebook.com/share/v/1AbCdEfGh/",
            "https://fb.watch/abcDEF123/",
            "https://www.instagram.com/reel/Cabc123/",
            "https://www.instagram.com/someone/reel/Cabc123/",
            "https://www.tiktok.com/@someone/video/7234567890123456789",
            "https://vm.tiktok.com/ZMabcdef/",
            "https://vimeo.com/76979871",
            "https://player.vimeo.com/video/76979871",
            "https://www.dailymotion.com/video/x8abcd",
            "https://www.twitch.tv/videos/1234567890",
            "https://www.twitch.tv/somechannel/clip/FunnyClipName",
            "https://v.redd.it/abc123xyz",
            "https://streamable.com/abc12",
        ] {
            assert!(is_page(page), "{page}");
        }
        // A post that may or may not carry a video is not claimed by its
        // address; the extension, which saw the player, says so.
        for other in [
            "https://x.com/historyinmemes",
            "https://x.com/home",
            "https://www.facebook.com/cnn",
            "https://www.facebook.com/cnn/posts/10155529876156509",
            "https://www.instagram.com/p/Cabc123/",
            "https://www.reddit.com/r/aww/comments/90bu6w/a_title/",
            "https://vimeo.com/channels/staffpicks",
            "https://www.twitch.tv/somechannel",
            "https://video.twimg.com/ext_tw_video/1/pu/vid/720x1280/a.mp4",
            "https://example.com/status/123",
        ] {
            assert!(!is_page(other), "{other}");
        }
    }

    #[test]
    fn a_post_with_files_the_site_does_not_measure() {
        // The shape X answers with: one file per size with both picture and
        // sound, no codec names, sizes only as estimates, and the same
        // sizes again as streams.
        let post = serde_json::json!({
            "_type": "video", "extractor_key": "Twitter", "title": "Historic Vids - One of the most intense moments https://t.co/Zgzhvix8ES",
            "uploader": "Historic Vids", "uploader_id": "historyinmemes", "duration": 15.488,
            "formats": [
                { "format_id": "hls-audio-128000-Audio", "protocol": "m3u8_native", "ext": "mp4", "vcodec": "none", "tbr": 128, "url": "https://video.example/a128.m3u8", "manifest_url": "https://video.example/master.m3u8" },
                { "format_id": "http-288", "protocol": "https", "ext": "mp4", "width": 480, "height": 270, "tbr": 288, "filesize_approx": 557568, "url": "https://video.example/270.mp4" },
                { "format_id": "hls-191", "protocol": "m3u8_native", "ext": "mp4", "vcodec": "avc1.4D400D", "acodec": "none", "width": 480, "height": 270, "tbr": 191.8, "url": "https://video.example/270.m3u8", "manifest_url": "https://video.example/master.m3u8" },
                { "format_id": "http-832", "protocol": "https", "ext": "mp4", "width": 640, "height": 360, "tbr": 832, "filesize_approx": 1610752, "url": "https://video.example/360.mp4" },
                { "format_id": "http-2176", "protocol": "https", "ext": "mp4", "width": 1280, "height": 720, "tbr": 2176, "url": "https://video.example/720.mp4" },
            ],
        });
        let resolved = parse(post.to_string().as_bytes()).unwrap();
        assert_eq!(resolved.site, "twitter");
        assert_eq!(resolved.name(), "Historic Vids - One of the most intense moments", "the account once, the text, no link");
        let choices = variants(&resolved);
        assert_eq!(choices.iter().map(|v| (v.id.as_str(), v.label.as_str(), v.detail.as_str())).collect::<Vec<_>>(), vec![("720", "720p", "MP4"), ("360", "360p", "MP4"), ("270", "270p", "MP4")]);
        let best = &choices[0];
        assert!(!best.is_hls() && !best.needs_joining() && best.audio.is_none(), "one file with picture and sound: nothing to join");
        // 2176 kbit/s for 15.488 s, since the site states no size at all.
        assert_eq!((best.size, best.size_estimated), (Some(4_212_736), true));
        assert_eq!((choices[1].size, choices[1].size_estimated), (Some(1_610_752), true));
        assert_eq!(pick(&choices, None, 1080).unwrap().id, "720");
        assert_eq!(pick(&choices, Some("360"), 1080).unwrap().id, "360");
    }

    #[test]
    fn upright_video_is_named_by_its_short_side() {
        let post = serde_json::json!({
            "title": "A reel", "uploader": "someone", "extractor_key": "Twitter", "duration": 10,
            "formats": [
                { "format_id": "http-950", "protocol": "https", "ext": "mp4", "width": 480, "height": 852, "tbr": 950, "url": "https://video.example/480.mp4" },
                { "format_id": "http-2176", "protocol": "https", "ext": "mp4", "width": 720, "height": 1280, "tbr": 2176, "url": "https://video.example/720.mp4" },
            ],
        });
        let choices = variants(&parse(post.to_string().as_bytes()).unwrap());
        assert_eq!(choices.iter().map(|v| v.label.as_str()).collect::<Vec<_>>(), vec!["720p", "480p"]);
        assert_eq!(pick(&choices, None, 720).unwrap().video.as_ref().unwrap().id, "http-2176", "a 720 by 1280 picture is 720p, not above it");
    }

    #[test]
    fn separate_picture_and_sound_and_files_without_a_size() {
        // The shape Facebook answers with.
        let post = serde_json::json!({
            "title": "Holocaust survivor becomes US citizen", "uploader": "CNN", "extractor_key": "Facebook", "duration": 44.181,
            "formats": [
                { "format_id": "1406a", "protocol": "https", "ext": "m4a", "vcodec": "none", "acodec": "mp4a.40.29", "tbr": 49.237, "url": "https://video.example/a.m4a", "format_note": "DASH audio" },
                { "format_id": "sd", "protocol": "https", "ext": "mp4", "quality": -3, "url": "https://video.example/sd.mp4" },
                { "format_id": "hd", "protocol": "https", "ext": "mp4", "quality": -2, "url": "https://video.example/hd.mp4" },
                { "format_id": "1740v", "protocol": "https", "ext": "mp4", "vcodec": "vp09.00.30.08", "acodec": "none", "height": 720, "tbr": 1358.794, "url": "https://video.example/720.mp4" },
                { "format_id": "1741v", "protocol": "https", "ext": "mp4", "vcodec": "vp09.00.40.08", "acodec": "none", "height": 1080, "tbr": 1872.083, "url": "https://video.example/1080.mp4" },
            ],
        });
        let resolved = parse(post.to_string().as_bytes()).unwrap();
        assert_eq!(resolved.name(), "CNN - Holocaust survivor becomes US citizen");
        let choices = variants(&resolved);
        assert_eq!(choices.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(), vec!["1080", "720", "hd", "sd", AUDIO_ONLY]);
        let full = &choices[0];
        assert_eq!((full.label.as_str(), full.detail.as_str(), full.container), ("1080p", "MP4  \u{00B7}  VP9", "mp4"));
        assert!(full.needs_joining());
        assert_eq!(full.audio.as_ref().unwrap().id, "1406a");
        assert!(full.size_estimated && full.size.is_some_and(|size| (10_000_000..11_000_000).contains(&size)), "{:?}", full.size);
        let hd = &choices[2];
        assert_eq!((hd.label.as_str(), hd.height, hd.size), ("HD", 0, None));
        assert!(!hd.needs_joining(), "codecs the site does not name are taken to be there");
        // Sizes come first; the unnamed files only when there is nothing else.
        assert_eq!(pick(&choices, None, 480).unwrap().id, "720", "nothing at or below 480: the smallest measured picture");
        assert_eq!(pick(&choices[2..], None, 1080).unwrap().id, "hd");
        assert_eq!(pick(&choices, Some("sd"), 1080).unwrap().id, "sd");
    }

    #[test]
    fn a_site_that_only_streams_and_a_post_with_several_videos() {
        let stream = |id: &str, height: u32, rate: f64| serde_json::json!({ "format_id": id, "protocol": "m3u8_native", "ext": "mp4", "vcodec": "avc1.64001F", "acodec": "mp4a.40.2", "width": height * 16 / 9, "height": height, "tbr": rate, "url": format!("https://stream.example/{height}/index.m3u8"), "manifest_url": "https://stream.example/master.m3u8" });
        let page = serde_json::json!({
            "_type": "playlist", "title": "Two clips", "uploader": "Someone",
            "entries": [
                { "title": "First clip", "extractor_key": "Vimeo", "duration": 100, "formats": [stream("hls-1", 1080, 4000.0), stream("hls-2", 720, 2000.0), stream("hls-3", 720, 1500.0)] },
                { "title": "Second clip", "formats": [stream("hls-9", 360, 500.0)] },
            ],
        });
        let resolved = parse(page.to_string().as_bytes()).unwrap();
        assert_eq!((resolved.title.as_str(), resolved.name().as_str()), ("First clip", "First clip"), "the first video, under its own title");
        let choices = variants(&resolved);
        assert_eq!(choices.iter().map(|v| (v.id.as_str(), v.is_hls(), v.container)).collect::<Vec<_>>(), vec![("1080", true, "mp4"), ("720", true, "mp4")]);
        assert_eq!(choices[1].video.as_ref().unwrap().id, "hls-2", "the better of two renditions of one size");
        assert_eq!(choices[0].video.as_ref().unwrap().manifest.as_deref(), Some("https://stream.example/master.m3u8"));
        assert!(!choices[0].needs_joining(), "the stream engine assembles it");
        assert_eq!(choices[0].detail, "MP4  \u{00B7}  H.264  \u{00B7}  stream");
    }

    #[test]
    fn names_are_made_for_files() {
        let named = |site: &str, uploader: Option<&str>, title: &str| Resolved { title: title.to_owned(), uploader: uploader.map(str::to_owned), duration: None, live: false, formats: Vec::new(), site: site.to_owned() }.name();
        assert_eq!(named("youtube", Some("Blender"), "Big Buck Bunny"), "Big Buck Bunny", "a titled video keeps its title");
        assert_eq!(named("facebook", Some("Val Phone"), "A phone that does it all"), "Val Phone - A phone that does it all");
        assert_eq!(named("instagram", Some("someone"), ""), "someone");
        assert_eq!(named("reddit", None, ""), "Video");
        let long = named("twitter", Some("Someone"), &"word ".repeat(60));
        assert!(long.chars().count() <= 97 && long.ends_with('\u{2026}') && !long.contains("  "), "{long}");
    }

    #[test]
    fn a_login_is_written_for_the_site_and_its_parents_only() {
        assert_eq!(cookie_domains("www.facebook.com"), vec![".www.facebook.com", ".facebook.com"]);
        assert_eq!(cookie_domains("x.com"), vec![".x.com"]);
        assert_eq!(cookie_domains("localhost"), vec!["localhost"]);
        assert_eq!(cookie_domains("127.0.0.1"), vec!["127.0.0.1"]);
        let path = {
            let file = CookieFile::write("www.reddit.com", "session=abc123; theme=dark; broken; tab\there=1").unwrap();
            let text = std::fs::read_to_string(&file.0).unwrap();
            assert!(text.starts_with("# Netscape HTTP Cookie File\n"));
            assert!(text.contains(".reddit.com\tTRUE\t/\tTRUE\t2147483647\tsession\tabc123\n"));
            assert!(text.contains(".www.reddit.com\tTRUE\t/\tTRUE\t2147483647\ttheme\tdark\n"));
            assert_eq!(text.lines().count(), 5, "two cookies for two domains; the malformed pairs are left out");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(std::fs::metadata(&file.0).unwrap().permissions().mode() & 0o777, 0o600);
            }
            file.0.clone()
        };
        assert!(!path.exists(), "the file is gone as soon as the helper has read it");
        assert!(may_use_login("https://www.reddit.com/r/aww/comments/1/x/"));
        assert!(!may_use_login("https://www.youtube.com/watch?v=aqz-KE-bpKQ"), "never a YouTube account");
        assert!(!may_use_login("https://youtu.be/aqz-KE-bpKQ"));
        assert_eq!(cookie_header("tt_chain_token=abc; Domain=.tiktok.com; Path=/; Secure; ttwid=xyz; Domain=.tiktok.com; Expires=1798761600").as_deref(), Some("tt_chain_token=abc; ttwid=xyz"));
        assert_eq!(cookie_header("Domain=.x.com; Secure"), None);
    }
}
