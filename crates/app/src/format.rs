//! Numbers and addresses, worded for people.

/// `1536` → `1.50 KB`. Binary multiples, three significant digits.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    // 1000..1023 of a unit would print four digits; keep the width stable.
    if value >= 999.5 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let digits = if value >= 100.0 { 0 } else if value >= 10.0 { 1 } else { 2 };
    format!("{value:.digits$} {}", UNITS[unit])
}

/// Bytes per second as `12.4 MB/s`.
pub fn speed(bytes_per_sec: f64) -> String {
    if bytes_per_sec < 1.0 {
        return "0 B/s".to_owned();
    }
    format!("{}/s", bytes(bytes_per_sec.round() as u64))
}

/// Seconds as the two most significant units: `2h 05m`, `3m 12s`, `8s`.
pub fn duration(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60, secs % 60);
    if d > 0 {
        format!("{d}d {h:02}h")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// `0.4273` → `42.7%`. Never rounds an unfinished download up to 100.
pub fn percent(fraction: f32) -> String {
    let pct = (fraction.clamp(0.0, 1.0) * 100.0) as f64;
    if pct >= 100.0 {
        "100%".to_owned()
    } else {
        format!("{:.1}%", (pct * 10.0).floor() / 10.0)
    }
}

/// How long ago a Unix timestamp was, coarsely.
pub fn ago(then: u64, now: u64) -> String {
    let secs = now.saturating_sub(then);
    match secs {
        0..=59 => "just now".to_owned(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => {
            let h = secs / 3600;
            format!("{h} hour{} ago", if h == 1 { "" } else { "s" })
        }
        _ => {
            let d = secs / 86_400;
            format!("{d} day{} ago", if d == 1 { "" } else { "s" })
        }
    }
}

/// The host of an address, without `www.` and without credentials.
pub fn host(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.strip_prefix("www.").unwrap_or(host);
    host.to_owned()
}

/// An address with any `user:password@` removed, for display.
pub fn safe_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, tail) = match rest.find(['/', '?', '#']) {
                Some(i) => rest.split_at(i),
                None => (rest, ""),
            };
            let host = authority.rsplit('@').next().unwrap_or(authority);
            format!("{scheme}://{host}{tail}")
        }
        None => url.to_owned(),
    }
}

/// A folder path with the home directory written as `~`.
pub fn home_relative(path: &std::path::Path) -> String {
    let full = path.to_string_lossy().into_owned();
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_default();
    match full.strip_prefix(&home) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with(['/', '\\'])) => format!("~{rest}"),
        _ => full,
    }
}

/// What an HTTP status means for a link that stopped working.
pub fn expiry_reason(code: u16) -> &'static str {
    match code {
        401 => "The server asks for a sign-in",
        403 => "The link expired or access was denied",
        404 => "The file is no longer at this address",
        410 => "The link has expired",
        550 => "The file is not available on the server",
        _ => "The link stopped working",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_sizes() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1024), "1.00 KB");
        assert_eq!(bytes(1536), "1.50 KB");
        assert_eq!(bytes(10 * 1024 * 1024), "10.0 MB");
        assert_eq!(bytes(4_500_000_000), "4.19 GB");
        // 1023 MB would need four digits; it reads as a gigabyte instead.
        assert_eq!(bytes(1023 * 1024 * 1024), "1.00 GB");
        assert_eq!(bytes(250 * 1024 * 1024), "250 MB");
    }

    #[test]
    fn speeds_and_durations() {
        assert_eq!(speed(0.0), "0 B/s");
        assert_eq!(speed(12.4 * 1024.0 * 1024.0), "12.4 MB/s");
        assert_eq!(duration(8), "8s");
        assert_eq!(duration(192), "3m 12s");
        assert_eq!(duration(7500), "2h 05m");
        assert_eq!(duration(200_000), "2d 07h");
    }

    #[test]
    fn percent_never_lies_about_completion() {
        assert_eq!(percent(0.4273), "42.7%");
        assert_eq!(percent(0.9999), "99.9%");
        assert_eq!(percent(1.0), "100%");
        assert_eq!(percent(0.0), "0.0%");
    }

    #[test]
    fn addresses() {
        assert_eq!(host("https://www.example.com/a/b?c"), "example.com");
        assert_eq!(host("ftp://user:pw@files.example.org:2121/x"), "files.example.org:2121");
        assert_eq!(safe_url("ftp://user:pw@files.example.org/x.zip"), "ftp://files.example.org/x.zip");
        assert_eq!(safe_url("https://a.example/b"), "https://a.example/b");
        assert_eq!(ago(100, 130), "just now");
        assert_eq!(ago(0, 7200), "2 hours ago");
        assert_eq!(ago(0, 86_400), "1 day ago");
    }
}
