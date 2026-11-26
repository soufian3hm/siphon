//! HTTP Live Streaming: turn a playlist into one media file.
//!
//! A stream is a list of small files. They are fetched concurrently by a
//! pool of workers (each finished segment is a file on disk, so a stopped
//! download continues with whatever is missing), then stitched together in
//! playlist order and, when `ffmpeg` is installed, rewrapped as a regular
//! MP4 without re-encoding.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aes::cipher::{BlockModeDecrypt, KeyIvInit, block_padding::Pkcs7};
use m3u8_rs::{AlternativeMediaType, KeyMethod, MediaPlaylist, Playlist};
use parking_lot::Mutex;
use reqwest::header;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};
use crate::filename;
use crate::rate::Limiter;
use crate::transfer::{Outcome, Progress, blocking};
use crate::types::Request;

const PARTS_EXT: &str = "siphon.parts";
const SEGMENT_RETRIES: u32 = 6;

/// One piece of the stream.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Piece {
    url: String,
    /// `(offset, length)` when the piece is a slice of a larger resource.
    range: Option<(u64, u64)>,
    key: Option<KeyRef>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct KeyRef {
    url: String,
    iv: [u8; 16],
}

/// One elementary playlist: an optional initialisation section and its
/// media segments.
#[derive(Clone, Debug, Default, PartialEq)]
struct Track {
    init: Option<Piece>,
    segments: Vec<Piece>,
    duration: f32,
}

/// Everything decided about a stream before a single segment is fetched.
#[derive(Clone, Debug)]
pub struct HlsPlan {
    request: Request,
    /// The media playlist the video comes from.
    pub media_url: String,
    video: Track,
    /// Audio delivered as its own rendition, when the stream splits them.
    audio: Option<Track>,
    pub resolution: Option<(u64, u64)>,
    pub bandwidth: Option<u64>,
    /// The picture qualities the stream comes in (1080, 720, ...), best
    /// first. Empty when the address was one rendition, not a master list.
    pub qualities: Vec<u32>,
    /// Segments are fragmented MP4 rather than MPEG-TS.
    fragmented_mp4: bool,
    ffmpeg: Option<PathBuf>,
}

impl HlsPlan {
    pub fn segment_count(&self) -> usize {
        let count = |t: &Track| t.segments.len() + t.init.is_some() as usize;
        count(&self.video) + self.audio.as_ref().map(count).unwrap_or(0)
    }

    pub fn duration_secs(&self) -> f32 {
        self.video.duration
    }

    /// The extension the finished file will really have.
    pub fn container(&self) -> &'static str {
        self.extension()
    }

    fn extension(&self) -> &'static str {
        if self.fragmented_mp4 || self.ffmpeg.is_some() { "mp4" } else { "ts" }
    }

    /// A name derived from the playlist address. Stream URLs rarely carry a
    /// useful one (`master.m3u8`, `index.m3u8`), so generic stems fall back
    /// to the folder they sit in.
    pub fn file_name(&self) -> String {
        const GENERIC: [&str; 9] =
            ["master", "index", "playlist", "chunklist", "manifest", "stream", "video", "main", "prog_index"];
        let segments: Vec<String> = url::Url::parse(&self.request.url)
            .ok()
            .and_then(|u| u.path_segments().map(|s| s.filter(|p| !p.is_empty()).map(str::to_owned).collect()))
            .unwrap_or_default();
        let stem_of = |s: &str| {
            let decoded = percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned();
            filename::split_extension(&decoded).0.to_owned()
        };
        let last = segments.last().map(|s| stem_of(s)).unwrap_or_default();
        let stem = if last.is_empty() || GENERIC.contains(&last.to_ascii_lowercase().as_str()) {
            segments.iter().rev().nth(1).map(|s| stem_of(s)).filter(|s| !s.is_empty()).unwrap_or_else(|| "video".into())
        } else {
            last
        };
        filename::sanitize(&format!("{stem}.{}", self.extension()))
    }

    /// One line for the interface: quality, length, how it will be saved.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some((_, height)) = self.resolution {
            parts.push(format!("{height}p"));
        }
        let secs = self.video.duration.round() as u64;
        if secs > 0 {
            parts.push(if secs >= 3600 {
                format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
            } else {
                format!("{}:{:02}", secs / 60, secs % 60)
            });
        }
        parts.push(format!("{} segments", self.segment_count()));
        if self.ffmpeg.is_none() && (!self.fragmented_mp4 || self.audio.is_some()) {
            parts.push("ffmpeg not found: saved without remuxing".into());
        }
        parts.join(" \u{00B7} ")
    }
}

/// Where `ffmpeg` lives, if it is installed.
pub fn find_ffmpeg() -> Option<PathBuf> {
    let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    // A GUI application launched from the Dock does not inherit the shell's PATH.
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/opt/local/bin"].map(PathBuf::from));
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

fn headers_for(request: &Request) -> header::HeaderMap {
    let mut headers = header::HeaderMap::new();
    headers.insert(header::ACCEPT, header::HeaderValue::from_static("*/*"));
    if let Some(referer) = request.referer.as_deref()
        && let Ok(value) = header::HeaderValue::from_str(referer)
    {
        headers.insert(header::REFERER, value);
    }
    for (name, value) in &request.headers {
        let lower = name.trim().to_ascii_lowercase();
        if matches!(lower.as_str(), "cookie" | "authorization" | "user-agent" | "origin")
            && let (Ok(name), Ok(value)) =
                (header::HeaderName::from_bytes(lower.as_bytes()), header::HeaderValue::from_str(value.trim()))
        {
            headers.insert(name, value);
        }
    }
    headers
}

async fn fetch_text(client: &reqwest::Client, request: &Request, url: &str) -> Result<(String, String)> {
    let response = client.get(url).headers(headers_for(request)).send().await?;
    if !response.status().is_success() {
        return Err(Error::from_status(response.status().as_u16(), None));
    }
    let final_url = response.url().to_string();
    let bytes = response.bytes().await?;
    // Playlists are a few kilobytes. Megabytes means this is not a playlist.
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(Error::Protocol("the playlist is implausibly large".into()));
    }
    Ok((String::from_utf8_lossy(&bytes).into_owned(), final_url))
}

fn resolve(base: &str, reference: &str) -> Result<String> {
    let base = url::Url::parse(base).map_err(|e| Error::InvalidUrl(e.to_string()))?;
    base.join(reference.trim()).map(|u| u.to_string()).map_err(|e| Error::InvalidUrl(format!("{reference}: {e}")))
}

/// Reads the playlist(s) and decides what will be downloaded: the best
/// picture the stream has.
pub async fn plan(client: &reqwest::Client, request: &Request) -> Result<HlsPlan> {
    plan_for(client, request, None).await
}

/// The quality a rendition is named by: the short side of its picture.
fn quality_of(variant: &m3u8_rs::VariantStream) -> u32 {
    variant.resolution.map(|r| r.width.min(r.height) as u32).unwrap_or(0)
}

/// Like [`plan`], for a chosen quality: the best rendition not above it, or
/// the smallest there is when all are above.
pub async fn plan_for(client: &reqwest::Client, request: &Request, quality: Option<u32>) -> Result<HlsPlan> {
    let (text, base) = fetch_text(client, request, request.url.trim()).await?;
    let playlist = m3u8_rs::parse_playlist_res(text.as_bytes())
        .map_err(|_| Error::Protocol("the address did not return a valid HLS playlist".into()))?;

    let mut resolution = None;
    let mut bandwidth = None;
    let mut audio = None;
    let mut qualities = Vec::new();

    let (media, media_url) = match playlist {
        Playlist::MediaPlaylist(media) => (media, base),
        Playlist::MasterPlaylist(master) => {
            let playable = || master.variants.iter().filter(|v| !v.is_i_frame && !v.uri.trim().is_empty());
            qualities = playable().map(quality_of).filter(|q| *q > 0).collect();
            qualities.sort_unstable_by(|a, b| b.cmp(a));
            qualities.dedup();
            let variant = match quality {
                Some(wanted) => playable()
                    .filter(|v| quality_of(v) <= wanted)
                    .max_by_key(|v| (quality_of(v), v.bandwidth))
                    .or_else(|| playable().min_by_key(|v| (quality_of(v), v.bandwidth))),
                // Best picture first, then best bitrate.
                None => playable().max_by_key(|v| (v.resolution.map(|r| r.height).unwrap_or(0), v.bandwidth)),
            }
            .ok_or_else(|| Error::Protocol("the playlist lists no playable stream".into()))?;
            resolution = variant.resolution.map(|r| (r.width, r.height));
            bandwidth = Some(variant.bandwidth);

            if let Some(group) = &variant.audio {
                let renditions = || {
                    master.alternatives.iter().filter(|a| {
                        a.media_type == AlternativeMediaType::Audio && &a.group_id == group && a.uri.is_some()
                    })
                };
                let chosen = renditions().find(|a| a.default).or_else(|| renditions().next());
                if let Some(uri) = chosen.and_then(|a| a.uri.as_deref()) {
                    let audio_url = resolve(&base, uri)?;
                    let (audio_text, audio_base) = fetch_text(client, request, &audio_url).await?;
                    let parsed = m3u8_rs::parse_media_playlist_res(audio_text.as_bytes())
                        .map_err(|_| Error::Protocol("the audio playlist could not be read".into()))?;
                    audio = Some(track(&parsed, &audio_base)?);
                }
            }

            let media_url = resolve(&base, &variant.uri)?;
            let (media_text, media_base) = fetch_text(client, request, &media_url).await?;
            let media = m3u8_rs::parse_media_playlist_res(media_text.as_bytes())
                .map_err(|_| Error::Protocol("the stream's segment list could not be read".into()))?;
            (media, media_base)
        }
    };

    let video = track(&media, &media_url)?;
    if video.segments.is_empty() {
        return Err(Error::Protocol("the playlist has no segments".into()));
    }
    let fragmented_mp4 = video.init.is_some()
        || video.segments.first().is_some_and(|s| {
            let path = s.url.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
            path.ends_with(".m4s") || path.ends_with(".mp4") || path.ends_with(".m4v") || path.ends_with(".cmfv")
        });

    Ok(HlsPlan {
        request: request.clone(),
        media_url,
        video,
        audio,
        resolution,
        bandwidth,
        qualities,
        fragmented_mp4,
        ffmpeg: find_ffmpeg(),
    })
}

/// Flattens a media playlist into absolute, self-describing pieces.
fn track(playlist: &MediaPlaylist, base: &str) -> Result<Track> {
    if !playlist.end_list {
        return Err(Error::Unsupported(
            "This is a live stream that has not ended; only finished streams can be saved.".into(),
        ));
    }

    let mut out = Track::default();
    let mut key: Option<(String, Option<[u8; 16]>)> = None;
    let mut next_offset: HashMap<String, u64> = HashMap::new();

    for (index, segment) in playlist.segments.iter().enumerate() {
        // `EXT-X-KEY` and `EXT-X-MAP` apply to every segment after them.
        //
        // m3u8-rs 6.0.1 rejects `#EXT-X-KEY:METHOD=NONE` (its IV check is
        // inverted) and hands the line back as an unknown tag. Missing it
        // would mean "decrypting" clear segments with the previous key.
        let cleared = segment.unknown_tags.iter().any(|t| {
            t.tag.eq_ignore_ascii_case("X-KEY")
                && t.rest.as_deref().is_some_and(|rest| {
                    rest.split(',').any(|attr| attr.trim().eq_ignore_ascii_case("METHOD=NONE"))
                })
        });
        if cleared {
            key = None;
        }
        if let Some(k) = &segment.key {
            key = match &k.method {
                KeyMethod::None => None,
                KeyMethod::AES128 => {
                    let uri = k.uri.as_deref().ok_or_else(|| Error::Protocol("the stream key has no address".into()))?;
                    let iv = k.iv.as_deref().map(parse_iv).transpose()?;
                    Some((resolve(base, uri)?, iv))
                }
                KeyMethod::SampleAES | KeyMethod::Other(_) => {
                    return Err(Error::Unsupported(
                        "This stream is protected by DRM and cannot be saved.".into(),
                    ));
                }
            };
        }
        if let Some(map) = &segment.map
            && out.init.is_none()
        {
            out.init = Some(Piece {
                url: resolve(base, &map.uri)?,
                range: map.byte_range.as_ref().map(|r| (r.offset.unwrap_or(0), r.length)),
                // The initialisation section of an encrypted stream is clear.
                key: None,
            });
        }

        let url = resolve(base, &segment.uri)?;
        let range = segment.byte_range.as_ref().map(|r| {
            // Without an offset, a sub-range continues where the previous
            // one of the same resource ended.
            let offset = r.offset.unwrap_or_else(|| next_offset.get(&url).copied().unwrap_or(0));
            next_offset.insert(url.clone(), offset + r.length);
            (offset, r.length)
        });
        let key = key.as_ref().map(|(url, iv)| KeyRef {
            url: url.clone(),
            // No IV in the playlist means the segment's sequence number is the IV.
            iv: iv.unwrap_or_else(|| {
                let mut iv = [0u8; 16];
                iv[8..].copy_from_slice(&(playlist.media_sequence + index as u64).to_be_bytes());
                iv
            }),
        });
        out.duration += segment.duration;
        out.segments.push(Piece { url, range, key });
    }
    Ok(out)
}

fn parse_iv(text: &str) -> Result<[u8; 16]> {
    let hex = text.trim().trim_start_matches("0x").trim_start_matches("0X");
    if hex.len() > 32 || hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Protocol("the stream key has a malformed IV".into()));
    }
    let padded = format!("{hex:0>32}");
    let mut iv = [0u8; 16];
    for (i, byte) in iv.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&padded[i * 2..i * 2 + 2], 16).map_err(|_| Error::Protocol("bad IV".into()))?;
    }
    Ok(iv)
}

pub(crate) fn parts_dir(final_path: &Path) -> PathBuf {
    let mut os = final_path.as_os_str().to_owned();
    os.push(".");
    os.push(PARTS_EXT);
    PathBuf::from(os)
}

/// Removes the segment cache of a stream download.
pub fn discard_parts(final_path: &Path) {
    let _ = std::fs::remove_dir_all(parts_dir(final_path));
}

/// A piece with the file it is cached in.
struct Job {
    piece: Piece,
    file: PathBuf,
}

struct Pool {
    client: reqwest::Client,
    request: Request,
    jobs: Vec<Job>,
    next: AtomicUsize,
    /// Held across the fetch on purpose: workers that need the same key wait
    /// for the first request instead of each sending their own.
    keys: tokio::sync::Mutex<HashMap<String, [u8; 16]>>,
    progress: Arc<Progress>,
    limiter: Arc<Limiter>,
    abort: CancellationToken,
    failure: Mutex<Option<Error>>,
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    client: &reqwest::Client,
    plan: HlsPlan,
    final_path: &Path,
    connections: usize,
    progress: &Arc<Progress>,
    limiter: &Arc<Limiter>,
    cancel: &CancellationToken,
    on_assembling: &(dyn Fn() + Send + Sync),
) -> Result<Outcome> {
    let dir = parts_dir(final_path);
    std::fs::create_dir_all(&dir)?;

    // A cache built from a different playlist (the stream was re-encoded, or
    // a fresh link points somewhere else) cannot be mixed with this one.
    let fingerprint = format!("{}:{}", plan.video.segments.len(), plan.audio.as_ref().map(|a| a.segments.len()).unwrap_or(0));
    let stamp = dir.join("layout");
    if std::fs::read_to_string(&stamp).ok().as_deref() != Some(fingerprint.as_str()) {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&stamp, &fingerprint)?;
    }

    let mut jobs = Vec::with_capacity(plan.segment_count());
    let mut video_files = Vec::new();
    let mut audio_files = Vec::new();
    for (prefix, track, files) in [("v", Some(&plan.video), &mut video_files), ("a", plan.audio.as_ref(), &mut audio_files)] {
        let Some(track) = track else { continue };
        if let Some(init) = &track.init {
            let file = dir.join(format!("{prefix}-init"));
            files.push(file.clone());
            jobs.push(Job { piece: init.clone(), file });
        }
        for (i, piece) in track.segments.iter().enumerate() {
            let file = dir.join(format!("{prefix}-{i:06}"));
            files.push(file.clone());
            jobs.push(Job { piece: piece.clone(), file });
        }
    }

    // What an earlier run already fetched counts as done.
    let total_units = jobs.len() as u32;
    let mut have_bytes = 0u64;
    jobs.retain(|job| match std::fs::metadata(&job.file) {
        Ok(meta) => {
            have_bytes += meta.len();
            false
        }
        Err(_) => true,
    });
    progress.reset();
    progress.set_units(total_units - jobs.len() as u32, total_units);
    progress.add_stream_bytes(have_bytes);

    let pool = Arc::new(Pool {
        client: client.clone(),
        request: plan.request.clone(),
        jobs,
        next: AtomicUsize::new(0),
        keys: tokio::sync::Mutex::new(HashMap::new()),
        progress: progress.clone(),
        limiter: limiter.clone(),
        abort: cancel.child_token(),
        failure: Mutex::new(None),
    });

    let mut workers = JoinSet::new();
    for _ in 0..connections.clamp(1, 16).min(pool.jobs.len().max(1)) {
        let pool = pool.clone();
        workers.spawn(async move {
            loop {
                let index = pool.next.fetch_add(1, Ordering::Relaxed);
                let Some(job) = pool.jobs.get(index) else { break };
                pool.progress.connection_opened();
                let result = fetch_piece(&pool, job).await;
                pool.progress.connection_closed();
                match result {
                    Ok(()) => pool.progress.unit_done(),
                    Err(Error::Cancelled) => break,
                    Err(e) => {
                        let mut slot = pool.failure.lock();
                        if slot.is_none() {
                            *slot = Some(e);
                        }
                        pool.abort.cancel();
                        break;
                    }
                }
            }
        });
    }
    while let Some(joined) = workers.join_next().await {
        if let Err(e) = joined {
            return Err(Error::Protocol(format!("a stream worker crashed: {e}")));
        }
    }
    if let Some(error) = pool.failure.lock().take() {
        return Err(error);
    }
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }

    on_assembling();
    let path = assemble(&plan, &dir, &video_files, &audio_files, final_path, cancel).await?;
    let bytes = std::fs::metadata(&path)?.len();
    let _ = std::fs::remove_dir_all(&dir);
    Ok(Outcome::Completed { path, bytes })
}

async fn fetch_piece(pool: &Pool, job: &Job) -> Result<()> {
    let mut failures = 0u32;
    loop {
        match fetch_piece_once(pool, job).await {
            Ok(()) => return Ok(()),
            Err(e) if e.is_transient() && failures < SEGMENT_RETRIES => {
                failures += 1;
                let wait = Duration::from_millis(300 * (1u64 << failures.min(5)));
                tokio::select! {
                    _ = pool.abort.cancelled() => return Err(Error::Cancelled),
                    _ = tokio::time::sleep(wait) => {}
                }
            }
            Err(e) => return Err(e),
        }
    }
}

async fn fetch_piece_once(pool: &Pool, job: &Job) -> Result<()> {
    let key = match &job.piece.key {
        Some(key_ref) => Some((key_bytes(pool, &key_ref.url).await?, key_ref.iv)),
        None => None,
    };

    let mut request = pool.client.get(&job.piece.url).headers(headers_for(&pool.request));
    if let Some((offset, length)) = job.piece.range {
        request = request.header(header::RANGE, format!("bytes={}-{}", offset, offset + length - 1));
    }
    let mut response = tokio::select! {
        _ = pool.abort.cancelled() => return Err(Error::Cancelled),
        sent = request.send() => sent?,
    };
    if !response.status().is_success() {
        return Err(Error::from_status(response.status().as_u16(), None));
    }

    let mut data: Vec<u8> = Vec::with_capacity(response.content_length().unwrap_or(0).min(64 << 20) as usize);
    loop {
        let chunk = tokio::select! {
            _ = pool.abort.cancelled() => return Err(Error::Cancelled),
            chunk = response.chunk() => chunk?,
        };
        let Some(chunk) = chunk else { break };
        pool.progress.add_net(chunk.len() as u64);
        pool.limiter.take(chunk.len()).await;
        data.extend_from_slice(&chunk);
    }
    if let Some(expected) = response.content_length()
        && (data.len() as u64) < expected
    {
        return Err(Error::Truncated);
    }

    if let Some((key, iv)) = key {
        let plain_len = cbc::Decryptor::<aes::Aes128>::new(&key.into(), &iv.into())
            .decrypt_padded::<Pkcs7>(&mut data)
            .map_err(|_| Error::Protocol("a stream segment could not be decrypted".into()))?
            .len();
        data.truncate(plain_len);
    }

    // Written under a temporary name: a segment file either exists whole or
    // not at all, which is what makes "the file exists" mean "done".
    let tmp = job.file.with_extension("tmp");
    let len = data.len() as u64;
    blocking(|| std::fs::write(&tmp, &data).and_then(|()| std::fs::rename(&tmp, &job.file)))?;
    pool.progress.add_stream_bytes(len);
    Ok(())
}

async fn key_bytes(pool: &Pool, url: &str) -> Result<[u8; 16]> {
    let mut keys = pool.keys.lock().await;
    if let Some(key) = keys.get(url) {
        return Ok(*key);
    }
    let response = pool.client.get(url).headers(headers_for(&pool.request)).send().await?;
    if !response.status().is_success() {
        return Err(Error::from_status(response.status().as_u16(), None));
    }
    let bytes = response.bytes().await?;
    let key: [u8; 16] = bytes
        .as_ref()
        .try_into()
        .map_err(|_| Error::Protocol(format!("the stream key is {} bytes, expected 16", bytes.len())))?;
    keys.insert(url.to_owned(), key);
    Ok(key)
}

/// Appends files, in order, into one.
fn concatenate(parts: &[PathBuf], out: &Path) -> std::io::Result<()> {
    let mut writer = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(out)?);
    for part in parts {
        std::io::copy(&mut std::fs::File::open(part)?, &mut writer)?;
    }
    use std::io::Write;
    writer.flush()?;
    writer.get_ref().sync_data()
}

async fn assemble(
    plan: &HlsPlan,
    dir: &Path,
    video_files: &[PathBuf],
    audio_files: &[PathBuf],
    final_path: &Path,
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    let raw_ext = if plan.fragmented_mp4 { "mp4" } else { "ts" };
    let video_raw = dir.join(format!("video.{raw_ext}"));
    let audio_raw = (!audio_files.is_empty()).then(|| dir.join(format!("audio.{raw_ext}")));
    {
        let (video_files, audio_files) = (video_files.to_vec(), audio_files.to_vec());
        let (video_raw, audio_raw) = (video_raw.clone(), audio_raw.clone());
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            concatenate(&video_files, &video_raw)?;
            if let Some(audio_raw) = &audio_raw {
                concatenate(&audio_files, audio_raw)?;
            }
            Ok(())
        })
        .await
        .map_err(|e| Error::Disk(e.to_string()))??;
    }

    let (stem, _) = {
        let name = final_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "video".into());
        let (stem, ext) = filename::split_extension(&name);
        (stem.to_owned(), ext.to_owned())
    };
    let parent = final_path.parent().unwrap_or(Path::new("."));
    let place = |name: String| -> PathBuf { parent.join(filename::unique_in(parent, &name, &|_| false)) };

    if let Some(ffmpeg) = &plan.ffmpeg {
        let muxed = dir.join("muxed.mp4");
        let mut command = tokio::process::Command::new(ffmpeg);
        command.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"]).arg(&video_raw);
        if let Some(audio_raw) = &audio_raw {
            command.arg("-i").arg(audio_raw).args(["-map", "0:v:0?", "-map", "1:a:0?"]);
        }
        command.args(["-c", "copy"]);
        if !plan.fragmented_mp4 {
            // ADTS AAC from a transport stream needs its headers converted for MP4.
            command.args(["-bsf:a", "aac_adtstoasc"]);
        }
        command.args(["-movflags", "+faststart"]).arg(&muxed);
        command.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).kill_on_drop(true);

        let ran = tokio::select! {
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            output = command.output() => output,
        };
        if matches!(&ran, Ok(output) if output.status.success()) && muxed.is_file() {
            let target = place(format!("{stem}.mp4"));
            crate::disk::move_file(&muxed, &target)?;
            return Ok(target);
        }
        // Remuxing failed (an exotic codec, a broken ffmpeg). The stitched
        // stream is still perfectly playable; keep that instead of failing.
    }

    let target = place(format!("{stem}.{raw_ext}"));
    crate::disk::move_file(&video_raw, &target)?;
    if let Some(audio_raw) = &audio_raw {
        let audio_ext = if plan.fragmented_mp4 { "m4a" } else { "aac.ts" };
        crate::disk::move_file(audio_raw, &place(format!("{stem}.audio.{audio_ext}")))?;
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media(text: &str) -> MediaPlaylist {
        m3u8_rs::parse_media_playlist_res(text.as_bytes()).unwrap()
    }

    #[test]
    fn flattens_a_plain_playlist() {
        let pl = media(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:10\n#EXTINF:9.5,\nseg0.ts\n#EXTINF:8.0,\n../other/seg1.ts?x=1\n#EXT-X-ENDLIST\n",
        );
        let t = track(&pl, "https://cdn.example/v/720/index.m3u8").unwrap();
        assert_eq!(t.segments.len(), 2);
        assert_eq!(t.segments[0].url, "https://cdn.example/v/720/seg0.ts");
        assert_eq!(t.segments[1].url, "https://cdn.example/v/other/seg1.ts?x=1");
        assert!(t.init.is_none() && t.segments.iter().all(|s| s.key.is_none() && s.range.is_none()));
        assert!((t.duration - 17.5).abs() < 0.01);
    }

    #[test]
    fn keys_and_maps_carry_forward_and_ivs_default_to_the_sequence_number() {
        let pl = media(concat!(
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:7\n",
            "#EXT-X-MAP:URI=\"init.mp4\"\n",
            "#EXT-X-KEY:METHOD=AES-128,URI=\"https://keys.example/k1\"\n",
            "#EXTINF:6,\na.m4s\n#EXTINF:6,\nb.m4s\n",
            "#EXT-X-KEY:METHOD=AES-128,URI=\"k2\",IV=0x000102030405060708090A0B0C0D0E0F\n",
            "#EXTINF:6,\nc.m4s\n",
            "#EXT-X-KEY:METHOD=NONE\n",
            "#EXTINF:6,\nd.m4s\n#EXT-X-ENDLIST\n",
        ));
        let t = track(&pl, "https://cdn.example/s/main.m3u8").unwrap();
        assert_eq!(t.init.as_ref().unwrap().url, "https://cdn.example/s/init.mp4");

        let k0 = t.segments[0].key.as_ref().unwrap();
        assert_eq!(k0.url, "https://keys.example/k1");
        assert_eq!(k0.iv[15], 7, "sequence number 7");
        assert_eq!(t.segments[1].key.as_ref().unwrap().iv[15], 8);

        let k2 = t.segments[2].key.as_ref().unwrap();
        assert_eq!(k2.url, "https://cdn.example/s/k2");
        assert_eq!(k2.iv, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
        assert!(t.segments[3].key.is_none(), "METHOD=NONE ends encryption");
    }

    #[test]
    fn byte_ranges_continue_from_the_previous_one() {
        let pl = media(concat!(
            "#EXTM3U\n#EXT-X-VERSION:4\n#EXT-X-TARGETDURATION:5\n",
            "#EXTINF:5,\n#EXT-X-BYTERANGE:1000@0\nall.ts\n",
            "#EXTINF:5,\n#EXT-X-BYTERANGE:500\nall.ts\n",
            "#EXTINF:5,\n#EXT-X-BYTERANGE:250\nall.ts\n#EXT-X-ENDLIST\n",
        ));
        let t = track(&pl, "https://cdn.example/x.m3u8").unwrap();
        let ranges: Vec<_> = t.segments.iter().map(|s| s.range.unwrap()).collect();
        assert_eq!(ranges, vec![(0, 1000), (1000, 500), (1500, 250)]);
    }

    #[test]
    fn refuses_live_and_drm_streams() {
        let live = media("#EXTM3U\n#EXT-X-TARGETDURATION:5\n#EXTINF:5,\na.ts\n");
        assert!(matches!(track(&live, "https://x.example/l.m3u8"), Err(Error::Unsupported(_))));

        let drm = media(
            "#EXTM3U\n#EXT-X-TARGETDURATION:5\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\"\n#EXTINF:5,\na.ts\n#EXT-X-ENDLIST\n",
        );
        assert!(matches!(track(&drm, "https://x.example/d.m3u8"), Err(Error::Unsupported(_))));
    }

    #[test]
    fn iv_parsing() {
        assert_eq!(parse_iv("0x01").unwrap()[15], 1);
        assert_eq!(parse_iv("0XFF00000000000000000000000000000A").unwrap()[0], 0xFF);
        assert!(parse_iv("0xZZ").is_err());
        assert!(parse_iv(&format!("0x{}", "0".repeat(34))).is_err());
    }

    #[test]
    fn names_skip_generic_stems() {
        let plan = |url: &str, ffmpeg: bool| HlsPlan {
            request: Request::new(url),
            media_url: url.into(),
            video: Track::default(),
            audio: None,
            resolution: Some((1920, 1080)),
            bandwidth: None,
            qualities: Vec::new(),
            fragmented_mp4: false,
            ffmpeg: ffmpeg.then(|| PathBuf::from("/usr/bin/ffmpeg")),
        };
        assert_eq!(plan("https://x.example/shows/big-buck-bunny/master.m3u8?t=1", true).file_name(), "big-buck-bunny.mp4");
        assert_eq!(plan("https://x.example/trailer.m3u8", true).file_name(), "trailer.mp4");
        assert_eq!(plan("https://x.example/index.m3u8", false).file_name(), "video.ts");
    }
}
