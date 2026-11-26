//! Shared vocabulary of the engine.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Stable identifier of a download, unique for the lifetime of the library.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DownloadId(pub u64);

impl std::fmt::Display for DownloadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// How the bytes are fetched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Http,
    Ftp,
    /// Microsoft Media Server, carried over HTTP (MMSH).
    Mms,
    /// HTTP Live Streaming playlist assembled into one media file.
    Hls,
    /// A page with a player on it. A helper finds the streams behind it;
    /// they are fetched as files and joined.
    Page,
}

impl Protocol {
    pub fn label(self) -> &'static str {
        match self {
            Protocol::Http => "HTTP",
            Protocol::Ftp => "FTP",
            Protocol::Mms => "MMS",
            Protocol::Hls => "HLS",
            Protocol::Page => "Video page",
        }
    }
}

/// Everything needed to ask a server for the file.
///
/// The *identity* of a download (its destination, size and byte map) lives in
/// the record and the sidecar; this struct is the replaceable part. Swapping
/// an expired link means swapping this value and nothing else.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// The direct URL the bytes are fetched from.
    pub url: String,
    /// The page that produced the link. Sent as `Referer` and kept so the
    /// user can go back and mint a fresh link when this one expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referer: Option<String>,
    /// Extra request headers captured from the browser (`Cookie`,
    /// `Authorization`, `User-Agent`, ...). Names are case-insensitive.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    /// The address is a page with a player on it, not a file: the browser
    /// saw the video there. Such a page is read by the video helper even
    /// when its address alone would not say so.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub page: bool,
}

impl Request {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into(), referer: None, headers: Vec::new(), page: false }
    }

    /// Marks the address as a page with a video on it.
    pub fn as_page(mut self) -> Self {
        self.page = true;
        self
    }

    pub fn with_referer(mut self, referer: impl Into<String>) -> Self {
        let referer = referer.into();
        self.referer = (!referer.trim().is_empty()).then_some(referer);
        self
    }

    /// Sets a header, replacing any existing value with the same name.
    pub fn set_header(&mut self, name: &str, value: impl Into<String>) {
        self.headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
        self.headers.push((name.to_owned(), value.into()));
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn protocol(&self) -> Protocol {
        let web = self.url.trim().get(..4).is_some_and(|scheme| scheme.eq_ignore_ascii_case("http"));
        if self.page && web { Protocol::Page } else { Protocol::detect(&self.url) }
    }
}

impl Protocol {
    /// Classifies a URL by scheme and, for HTTP, by an HLS-looking path.
    pub fn detect(url: &str) -> Protocol {
        let lower = url.trim().to_ascii_lowercase();
        if lower.starts_with("ftp://") || lower.starts_with("ftps://") {
            return Protocol::Ftp;
        }
        if lower.starts_with("mms://") || lower.starts_with("mmsh://") || lower.starts_with("mmst://") {
            return Protocol::Mms;
        }
        if crate::extractor::is_page(url) {
            return Protocol::Page;
        }
        let path = lower.split(['?', '#']).next().unwrap_or("");
        if path.ends_with(".m3u8") || path.ends_with(".m3u") {
            return Protocol::Hls;
        }
        Protocol::Http
    }
}

/// Lifecycle of a download.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    /// Waiting for a free slot.
    Queued,
    /// Set aside for later: waits until the queue is started, by hand or by
    /// the scheduler.
    Scheduled,
    /// Talking to the server before any payload byte is requested.
    Connecting,
    Downloading,
    /// Stitching stream segments into the final file.
    Assembling,
    Paused,
    /// The server stopped honouring the link (`403`, `410`, ...). The partial
    /// file and its byte map are intact and waiting for a fresh URL.
    NeedsRefresh { code: u16 },
    Failed { message: String, retryable: bool },
    Completed,
}

impl Status {
    /// The download holds a slot and is doing work right now.
    pub fn is_active(&self) -> bool {
        matches!(self, Status::Connecting | Status::Downloading | Status::Assembling)
    }

    /// Nothing more will happen without the user (or a fresh link).
    pub fn is_stopped(&self) -> bool {
        matches!(
            self,
            Status::Paused | Status::Scheduled | Status::NeedsRefresh { .. } | Status::Failed { .. } | Status::Completed
        )
    }

    pub fn is_finished(&self) -> bool {
        matches!(self, Status::Completed)
    }

    /// The download can be told to (re)start.
    pub fn can_resume(&self) -> bool {
        matches!(self, Status::Paused | Status::Scheduled | Status::Failed { .. })
    }

    pub fn can_pause(&self) -> bool {
        matches!(self, Status::Queued | Status::Connecting | Status::Downloading)
    }
}

/// Coarse file family, derived from the extension. Drives filtering and icons.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Video,
    Audio,
    Image,
    Document,
    Archive,
    Program,
    Other,
}

impl Category {
    pub const ALL: [Category; 7] = [
        Category::Video,
        Category::Audio,
        Category::Image,
        Category::Document,
        Category::Archive,
        Category::Program,
        Category::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::Video => "Video",
            Category::Audio => "Audio",
            Category::Image => "Images",
            Category::Document => "Documents",
            Category::Archive => "Archives",
            Category::Program => "Programs",
            Category::Other => "Other",
        }
    }

    /// Name of the folder this family is sorted into, inside the download
    /// folder. Files of no particular family stay in the download folder.
    pub fn folder_name(self) -> Option<&'static str> {
        match self {
            Category::Video => Some("Video"),
            Category::Audio => Some("Music"),
            Category::Image => Some("Pictures"),
            Category::Document => Some("Documents"),
            Category::Archive => Some("Archives"),
            Category::Program => Some("Programs"),
            Category::Other => None,
        }
    }

    pub fn of(file_name: &str) -> Category {
        let ext = match file_name.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() => ext.to_ascii_lowercase(),
            _ => return Category::Other,
        };
        match ext.as_str() {
            "mp4" | "mkv" | "webm" | "mov" | "avi" | "wmv" | "flv" | "m4v" | "mpg" | "mpeg"
            | "ts" | "m2ts" | "3gp" | "ogv" | "asf" | "vob" | "rmvb" => Category::Video,
            "mp3" | "m4a" | "aac" | "flac" | "wav" | "ogg" | "opus" | "wma" | "aiff" | "aif"
            | "alac" | "mid" | "midi" | "mka" => Category::Audio,
            "jpg" | "jpeg" | "png" | "gif" | "webp" | "avif" | "heic" | "heif" | "bmp" | "tif"
            | "tiff" | "svg" | "ico" | "psd" | "raw" | "cr2" | "nef" | "dng" => Category::Image,
            "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "rtf" | "odt"
            | "ods" | "odp" | "csv" | "epub" | "mobi" | "md" | "pages" | "numbers" | "key"
            | "json" | "xml" | "html" | "htm" => Category::Document,
            "zip" | "rar" | "7z" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "lz4" | "cab"
            | "iso" | "img" | "lzma" | "z" | "tbz2" | "txz" => Category::Archive,
            "exe" | "msi" | "dmg" | "pkg" | "deb" | "rpm" | "apk" | "appimage" | "app" | "ipa"
            | "bat" | "sh" | "jar" | "msix" | "appx" | "run" | "bin" => Category::Program,
            _ => Category::Other,
        }
    }
}

/// One contiguous byte range of the file and how far it has been written.
///
/// Invariant: `start <= pos <= end`. `end` is exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentSnapshot {
    pub start: u64,
    pub pos: u64,
    pub end: u64,
    /// A worker currently owns this range.
    #[serde(default, skip_serializing)]
    pub active: bool,
}

impl SegmentSnapshot {
    pub fn len(&self) -> u64 {
        self.end - self.start
    }
    pub fn is_empty(&self) -> bool {
        self.end == self.start
    }
    pub fn done(&self) -> u64 {
        self.pos - self.start
    }
    pub fn remaining(&self) -> u64 {
        self.end - self.pos
    }
    pub fn is_complete(&self) -> bool {
        self.pos >= self.end
    }
}

/// Where a file ends up on disk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Destination {
    pub dir: PathBuf,
    pub file_name: String,
}

impl Destination {
    pub fn path(&self) -> PathBuf {
        self.dir.join(&self.file_name)
    }
}

/// Seconds since the Unix epoch, saturating at zero for clocks set before it.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_detection() {
        assert_eq!(Protocol::detect("https://a.example/file.zip"), Protocol::Http);
        assert_eq!(Protocol::detect("FTP://a.example/file.zip"), Protocol::Ftp);
        assert_eq!(Protocol::detect("mms://a.example/stream.wmv"), Protocol::Mms);
        assert_eq!(Protocol::detect("https://a.example/v/master.m3u8?tok=1"), Protocol::Hls);
        assert_eq!(Protocol::detect("https://a.example/get?file=x.m3u8"), Protocol::Http);
        assert_eq!(Protocol::detect("https://www.youtube.com/watch?v=aqz-KE-bpKQ"), Protocol::Page);
        assert_eq!(Protocol::detect("https://youtu.be/aqz-KE-bpKQ"), Protocol::Page);
        assert_eq!(Protocol::detect("https://www.youtube.com/"), Protocol::Http);
    }

    #[test]
    fn categories() {
        assert_eq!(Category::of("Movie.Final.MKV"), Category::Video);
        assert_eq!(Category::of("archive.tar.gz"), Category::Archive);
        assert_eq!(Category::of("setup.dmg"), Category::Program);
        assert_eq!(Category::of("README"), Category::Other);
        assert_eq!(Category::of(".gitignore"), Category::Other);
    }

    #[test]
    fn request_headers_are_case_insensitive() {
        let mut r = Request::new("https://a.example/x");
        r.set_header("cookie", "a=1");
        r.set_header("Cookie", "a=2");
        assert_eq!(r.headers.len(), 1);
        assert_eq!(r.header("COOKIE"), Some("a=2"));
    }
}
