//! File name intelligence: what should this download be called, and where
//! does it go without clobbering anything.

use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;

/// Extension of the in-progress data file. `movie.mkv` is written as
/// `movie.mkv.siphon` and renamed when the last byte lands.
pub const PART_EXT: &str = "siphon";
/// Extension of the sidecar holding the byte map and validators.
pub const META_EXT: &str = "siphon.meta";

const FALLBACK_NAME: &str = "download";
/// Keeps the full path comfortably under every mainstream filesystem limit.
const MAX_NAME_BYTES: usize = 180;

/// Extracts the file name from a `Content-Disposition` header value.
///
/// Implements the parts of RFC 6266 that servers actually send: the extended
/// `filename*=charset''percent-encoded` form (preferred), the quoted and bare
/// `filename=` forms, and the common non-standard habit of percent-encoding
/// the plain form.
pub fn from_content_disposition(value: &str) -> Option<String> {
    let mut plain: Option<String> = None;
    let mut extended: Option<String> = None;

    for param in split_params(value) {
        let Some((key, raw)) = param.split_once('=') else { continue };
        let key = key.trim().to_ascii_lowercase();
        let raw = raw.trim();
        match key.as_str() {
            "filename*" => {
                // charset'language'value
                let mut parts = raw.splitn(3, '\'');
                let charset = parts.next().unwrap_or("").trim().to_ascii_lowercase();
                let _lang = parts.next();
                if let Some(encoded) = parts.next() {
                    let bytes: Vec<u8> = percent_decode_str(encoded.trim_matches('"')).collect();
                    let decoded = match charset.as_str() {
                        "iso-8859-1" | "latin1" => bytes.iter().map(|&b| b as char).collect(),
                        _ => String::from_utf8_lossy(&bytes).into_owned(),
                    };
                    if !decoded.trim().is_empty() {
                        extended = Some(decoded);
                    }
                }
            }
            "filename" => {
                let unquoted = unquote(raw);
                // Chrome and Firefox both undo percent-encoding here when the
                // result is valid UTF-8; servers rely on it.
                let decoded = match percent_decode_str(&unquoted).decode_utf8() {
                    Ok(d) if unquoted.contains('%') => d.into_owned(),
                    _ => unquoted,
                };
                if !decoded.trim().is_empty() {
                    plain = Some(decoded);
                }
            }
            _ => {}
        }
    }

    extended.or(plain).map(|n| sanitize(&n))
}

/// Splits on `;` while respecting quoted strings.
fn split_params(value: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let mut escaped = false;
    for (i, c) in value.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if in_quotes => escaped = true,
            '"' => in_quotes = !in_quotes,
            ';' if !in_quotes => {
                out.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&value[start..]);
    out
}

fn unquote(raw: &str) -> String {
    let raw = raw.trim();
    if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        let inner = &raw[1..raw.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else {
                out.push(c);
            }
        }
        out
    } else {
        raw.to_owned()
    }
}

/// Derives a name from the URL: the last path segment, or failing that a
/// query value that looks like a file name.
pub fn from_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;

    let segment = parsed
        .path_segments()
        .and_then(|mut s| s.rfind(|seg| !seg.is_empty()))
        .map(|seg| percent_decode_str(seg).decode_utf8_lossy().into_owned());

    if let Some(seg) = &segment
        && has_extension(seg)
    {
        return Some(sanitize(seg));
    }

    // `/download?file=report.pdf` style endpoints.
    for (key, value) in parsed.query_pairs() {
        let key = key.to_ascii_lowercase();
        if matches!(key.as_str(), "filename" | "file" | "name" | "fn" | "title" | "f")
            && has_extension(&value)
        {
            let last = value.rsplit(['/', '\\']).next().unwrap_or(&value);
            return Some(sanitize(last));
        }
    }

    segment.filter(|s| !s.trim().is_empty()).map(|s| sanitize(&s))
}

fn has_extension(name: &str) -> bool {
    match name.rsplit_once('.') {
        Some((stem, ext)) => {
            !stem.is_empty()
                && (1..=8).contains(&ext.len())
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
        }
        None => false,
    }
}

/// The extension a media type implies, for servers that name the file badly.
pub fn extension_for_mime(mime: &str) -> Option<&'static str> {
    let essence = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    Some(match essence.as_str() {
        "application/zip" | "application/x-zip-compressed" => "zip",
        "application/x-7z-compressed" => "7z",
        "application/x-rar-compressed" | "application/vnd.rar" => "rar",
        "application/gzip" | "application/x-gzip" => "gz",
        "application/x-tar" => "tar",
        "application/x-bzip2" => "bz2",
        "application/x-xz" => "xz",
        "application/zstd" => "zst",
        "application/pdf" => "pdf",
        "application/json" => "json",
        "application/xml" | "text/xml" => "xml",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "application/vnd.ms-powerpoint" => "ppt",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
        "application/epub+zip" => "epub",
        "application/x-apple-diskimage" => "dmg",
        "application/vnd.android.package-archive" => "apk",
        "application/x-msdownload" | "application/vnd.microsoft.portable-executable" => "exe",
        "application/x-msi" => "msi",
        "application/x-iso9660-image" => "iso",
        "application/vnd.debian.binary-package" | "application/x-debian-package" => "deb",
        "application/x-rpm" => "rpm",
        "application/java-archive" => "jar",
        "application/vnd.apple.mpegurl" | "application/x-mpegurl" => "m3u8",
        "application/dash+xml" => "mpd",
        "text/plain" => "txt",
        "text/html" => "html",
        "text/csv" => "csv",
        "text/markdown" => "md",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/avif" => "avif",
        "image/svg+xml" => "svg",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        "image/heic" => "heic",
        "image/x-icon" | "image/vnd.microsoft.icon" => "ico",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/x-matroska" => "mkv",
        "video/quicktime" => "mov",
        "video/x-msvideo" => "avi",
        "video/x-ms-wmv" => "wmv",
        "video/x-ms-asf" | "application/vnd.ms-asf" => "asf",
        "video/x-flv" => "flv",
        "video/mp2t" => "ts",
        "video/mpeg" => "mpg",
        "video/3gpp" => "3gp",
        "audio/mpeg" => "mp3",
        "audio/mp4" | "audio/x-m4a" => "m4a",
        "audio/aac" => "aac",
        "audio/flac" | "audio/x-flac" => "flac",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/ogg" => "ogg",
        "audio/opus" => "opus",
        "audio/webm" => "weba",
        "audio/x-ms-wma" => "wma",
        _ => return None,
    })
}

/// Picks the best name available: header, then URL, then a generic fallback,
/// and repairs a missing extension from the media type.
pub fn resolve(content_disposition: Option<&str>, url: &str, mime: Option<&str>) -> String {
    resolve_from(content_disposition, &[url], mime)
}

/// Like [`resolve`], with several candidate URLs in order of preference
/// (typically the redirect target first, then the link the user clicked).
/// A URL whose last segment carries an extension beats one that does not.
pub fn resolve_from(content_disposition: Option<&str>, urls: &[&str], mime: Option<&str>) -> String {
    let from_urls = || {
        let names: Vec<String> = urls.iter().filter_map(|u| from_url(u)).collect();
        names.iter().find(|n| has_extension(n)).or(names.first()).cloned()
    };
    let mut name = content_disposition
        .and_then(from_content_disposition)
        .or_else(from_urls)
        .unwrap_or_else(|| FALLBACK_NAME.to_owned());

    if !has_extension(&name)
        && let Some(ext) = mime.and_then(extension_for_mime)
    {
        name.push('.');
        name.push_str(ext);
    }
    sanitize(&name)
}

/// Makes a name safe on Windows, macOS and Linux at once.
pub fn sanitize(name: &str) -> String {
    // Only the final component: a server must never choose the directory.
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);

    let mut out = String::with_capacity(name.len());
    let mut last_space = false;
    for c in name.chars() {
        let c = match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if c.is_control() => continue,
            // Bidi overrides can disguise `exe.jpg` style names.
            '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200B}'..='\u{200F}' | '\u{FEFF}' => {
                continue;
            }
            c if c.is_whitespace() => ' ',
            c => c,
        };
        if c == ' ' {
            if last_space {
                continue;
            }
            last_space = true;
        } else {
            last_space = false;
        }
        out.push(c);
    }

    // Windows rejects trailing dots and spaces; a leading dot hides the file.
    let trimmed = out.trim().trim_end_matches('.').trim_start_matches('.').trim();
    let mut out = if trimmed.is_empty() { FALLBACK_NAME.to_owned() } else { trimmed.to_owned() };

    let stem_upper = out.split('.').next().unwrap_or("").trim().to_ascii_uppercase();
    let reserved = matches!(stem_upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem_upper.len() == 4
            && (stem_upper.starts_with("COM") || stem_upper.starts_with("LPT"))
            && stem_upper.as_bytes()[3].is_ascii_digit());
    if reserved {
        out.insert(0, '_');
    }

    truncate_keeping_extension(&out, MAX_NAME_BYTES)
}

fn truncate_keeping_extension(name: &str, max_bytes: usize) -> String {
    if name.len() <= max_bytes {
        return name.to_owned();
    }
    let (stem, ext) = split_extension(name);
    let ext_part = if ext.is_empty() { String::new() } else { format!(".{ext}") };
    let budget = max_bytes.saturating_sub(ext_part.len()).max(1);
    let mut cut = budget.min(stem.len());
    while cut > 0 && !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{}", stem[..cut].trim_end(), ext_part)
}

/// Splits `archive.tar.gz` into (`archive`, `tar.gz`) and `movie.mkv` into
/// (`movie`, `mkv`). Names without a usable extension return an empty one.
pub fn split_extension(name: &str) -> (&str, &str) {
    const DOUBLE: [&str; 6] = [".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst", ".tar.lz4", ".tar.z"];
    let lower = name.to_ascii_lowercase();
    for d in DOUBLE {
        if lower.ends_with(d) && name.len() > d.len() {
            let at = name.len() - d.len();
            return (&name[..at], &name[at + 1..]);
        }
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if has_extension(name) => (stem, ext),
        _ => (name, ""),
    }
}

/// Path of the in-progress data file for a final path.
pub fn part_path(final_path: &Path) -> PathBuf {
    append_ext(final_path, PART_EXT)
}

/// Path of the resume sidecar for a final path.
pub fn meta_path(final_path: &Path) -> PathBuf {
    append_ext(final_path, META_EXT)
}

fn append_ext(path: &Path, ext: &str) -> PathBuf {
    let mut os = path.as_os_str().to_owned();
    os.push(".");
    os.push(ext);
    PathBuf::from(os)
}

/// Returns a file name in `dir` that collides with nothing: no finished file,
/// no partial file, and none of the names in `reserved` (downloads that have
/// been accepted but have not touched the disk yet).
pub fn unique_in(dir: &Path, wanted: &str, reserved: &dyn Fn(&Path) -> bool) -> String {
    let taken = |name: &str| {
        let path = dir.join(name);
        path.exists() || part_path(&path).exists() || reserved(&path)
    };
    if !taken(wanted) {
        return wanted.to_owned();
    }
    let (stem, ext) = split_extension(wanted);
    for n in 1..10_000u32 {
        let candidate = if ext.is_empty() {
            format!("{stem} ({n})")
        } else {
            format!("{stem} ({n}).{ext}")
        };
        if !taken(&candidate) {
            return candidate;
        }
    }
    // Ten thousand collisions means something else is wrong; stay unique anyway.
    format!("{}-{}", crate::types::unix_now(), wanted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_forms() {
        assert_eq!(
            from_content_disposition(r#"attachment; filename="report final.pdf""#).as_deref(),
            Some("report final.pdf")
        );
        assert_eq!(
            from_content_disposition("attachment; filename=plain.zip").as_deref(),
            Some("plain.zip")
        );
        assert_eq!(
            from_content_disposition(
                r#"attachment; filename="fallback.zip"; filename*=UTF-8''na%C3%AFve%20file.zip"#
            )
            .as_deref(),
            Some("naïve file.zip")
        );
        assert_eq!(
            from_content_disposition(r#"attachment; filename="semi;colon.txt""#).as_deref(),
            Some("semi;colon.txt")
        );
        assert_eq!(
            from_content_disposition(r#"attachment; filename="my%20movie.mp4""#).as_deref(),
            Some("my movie.mp4")
        );
        assert_eq!(
            from_content_disposition(r#"inline; filename="quo\"te.txt""#).as_deref(),
            Some("quo_te.txt")
        );
        assert_eq!(from_content_disposition("attachment"), None);
    }

    #[test]
    fn a_server_cannot_pick_the_directory() {
        assert_eq!(
            from_content_disposition(r#"attachment; filename="../../etc/passwd""#).as_deref(),
            Some("passwd")
        );
        assert_eq!(sanitize(r"C:\Windows\System32\evil.dll"), "evil.dll");
        assert_eq!(sanitize(".bashrc"), "bashrc");
    }

    #[test]
    fn url_names() {
        assert_eq!(from_url("https://x.example/a/b/My%20File.zip?token=1").as_deref(), Some("My File.zip"));
        assert_eq!(from_url("https://x.example/download?file=report.pdf&x=1").as_deref(), Some("report.pdf"));
        assert_eq!(from_url("https://x.example/get/12345").as_deref(), Some("12345"));
        assert_eq!(from_url("https://x.example/"), None);
    }

    #[test]
    fn resolve_repairs_extension() {
        assert_eq!(resolve(None, "https://x.example/get/12345", Some("application/pdf")), "12345.pdf");
        assert_eq!(resolve(None, "https://x.example/", Some("video/mp4; codecs=avc1")), "download.mp4");
        assert_eq!(resolve(None, "https://x.example/a.zip", Some("text/html")), "a.zip");
        assert_eq!(
            resolve(Some(r#"attachment; filename="x.iso""#), "https://x.example/a.zip", None),
            "x.iso"
        );
        // A signed redirect target with an opaque path loses to the clicked link.
        assert_eq!(
            resolve_from(None, &["https://cdn.example/blob/9f8e7d?sig=1", "https://x.example/dl/setup.exe"], None),
            "setup.exe"
        );
    }

    #[test]
    fn sanitize_rules() {
        assert_eq!(sanitize("a<b>c:d|e?f*g.txt"), "a_b_c_d_e_f_g.txt");
        assert_eq!(sanitize("  spaced   out .txt.. "), "spaced out .txt");
        assert_eq!(sanitize("CON.txt"), "_CON.txt");
        assert_eq!(sanitize("com1"), "_com1");
        assert_eq!(sanitize("common.txt"), "common.txt");
        assert_eq!(sanitize(""), "download");
        assert_eq!(sanitize("photo\u{202E}gpj.exe"), "photogpj.exe");
        let long = format!("{}.mkv", "é".repeat(400));
        let cut = sanitize(&long);
        assert!(cut.len() <= MAX_NAME_BYTES && cut.ends_with(".mkv"));
    }

    #[test]
    fn extension_splitting() {
        assert_eq!(split_extension("archive.tar.gz"), ("archive", "tar.gz"));
        assert_eq!(split_extension("movie.mkv"), ("movie", "mkv"));
        assert_eq!(split_extension("README"), ("README", ""));
        assert_eq!(split_extension("v1.2 release notes"), ("v1.2 release notes", ""));
    }

    #[test]
    fn unique_names_skip_partials_and_reservations() {
        let dir = std::env::temp_dir().join(format!("siphon-unique-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.zip"), b"x").unwrap();
        std::fs::write(dir.join("a (1).zip.siphon"), b"x").unwrap();
        let reserved_path = dir.join("a (2).zip");
        let reserved = |p: &Path| p == reserved_path;
        assert_eq!(unique_in(&dir, "a.zip", &reserved), "a (3).zip");
        assert_eq!(unique_in(&dir, "b.tar.gz", &reserved), "b.tar.gz");
        std::fs::write(dir.join("b.tar.gz"), b"x").unwrap();
        assert_eq!(unique_in(&dir, "b.tar.gz", &reserved), "b (1).tar.gz");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
