//! What happens to a file the moment it is complete: it is labelled as a
//! download, optionally given the server's date, and optionally handed to a
//! command of the user's choosing.

use std::path::Path;
use std::time::SystemTime;

/// Where a finished file came from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Origin {
    pub url: String,
    pub referer: Option<String>,
    /// The server's `Last-Modified`, as it sent it.
    pub last_modified: Option<String>,
}

/// Marks a file as downloaded from the internet, the way a browser does.
///
/// On macOS this sets the quarantine attribute, so Gatekeeper checks an
/// application or installer before it first runs, and records the source
/// address, which Finder shows under "Where from". Elsewhere it does nothing.
pub fn mark_downloaded(path: &Path, origin: &Origin) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // "0083": downloaded from the internet, not yet opened. Then the
        // time in hexadecimal seconds, the downloading application, and the
        // id of a quarantine event (none is recorded).
        let quarantine = format!("0083;{:08x};Siphon;", crate::types::unix_now());
        xattr::set(path, "com.apple.quarantine", quarantine.as_bytes())?;

        let mut sources = vec![plist::Value::String(origin.url.clone())];
        if let Some(referer) = origin.referer.as_deref().filter(|r| !r.trim().is_empty()) {
            sources.push(plist::Value::String(referer.to_owned()));
        }
        let mut encoded = Vec::new();
        plist::Value::Array(sources).to_writer_binary(&mut encoded).map_err(std::io::Error::other)?;
        xattr::set(path, "com.apple.metadata:kMDItemWhereFroms", &encoded)?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (path, origin);
    }
    Ok(())
}

/// Gives the file the modification time the server reported for it.
pub fn keep_server_date(path: &Path, last_modified: &str) -> std::io::Result<()> {
    let when = httpdate::parse_http_date(last_modified.trim()).map_err(std::io::Error::other)?;
    // A date in the future is a misconfigured server; "now" is more honest.
    if when > SystemTime::now() {
        return Ok(());
    }
    std::fs::File::options().write(true).open(path)?.set_modified(when)
}

/// Starts the user's command for a finished file without waiting for it.
///
/// `[File]` in the command stands for the file. The path is passed as an
/// argument rather than pasted into the command text, so a file name with
/// quotes or spaces cannot change what runs.
pub fn run_command(command: &str, path: &Path, origin: &Origin) -> std::io::Result<()> {
    let command = command.trim();
    if command.is_empty() {
        return Ok(());
    }
    let mut process;
    #[cfg(unix)]
    {
        let script = if command.contains("[File]") { command.replace("[File]", "\"$1\"") } else { format!("{command} \"$1\"") };
        process = std::process::Command::new("/bin/sh");
        process.arg("-c").arg(script).arg("siphon").arg(path);
    }
    #[cfg(not(unix))]
    {
        let script = if command.contains("[File]") { command.replace("[File]", "\"%SIPHON_FILE%\"") } else { format!("{command} \"%SIPHON_FILE%\"") };
        process = std::process::Command::new("cmd");
        process.arg("/C").arg(script);
    }
    process
        .env("SIPHON_FILE", path)
        .env("SIPHON_URL", &origin.url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("siphon-finishing-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_server_date_becomes_the_file_date() {
        let dir = scratch("date");
        let file = dir.join("a.bin");
        std::fs::write(&file, b"x").unwrap();
        keep_server_date(&file, "Wed, 21 Oct 2015 07:28:00 GMT").unwrap();
        let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
        assert_eq!(modified.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs(), 1_445_412_480);

        // Nonsense and future dates leave the file alone.
        assert!(keep_server_date(&file, "yesterday-ish").is_err());
        let future = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(86_400 * 400));
        keep_server_date(&file, &future).unwrap();
        let unchanged = std::fs::metadata(&file).unwrap().modified().unwrap();
        assert_eq!(unchanged, modified);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn finished_files_are_labelled_like_browser_downloads() {
        let dir = scratch("mark");
        let file = dir.join("installer.dmg");
        std::fs::write(&file, b"x").unwrap();
        let origin = Origin { url: "https://cdn.example/installer.dmg?sig=1".into(), referer: Some("https://example.com/download".into()), last_modified: None };
        mark_downloaded(&file, &origin).unwrap();

        let quarantine = String::from_utf8(xattr::get(&file, "com.apple.quarantine").unwrap().unwrap()).unwrap();
        let fields: Vec<&str> = quarantine.split(';').collect();
        assert_eq!(fields.len(), 4);
        assert_eq!(fields[0], "0083");
        assert!(u64::from_str_radix(fields[1], 16).unwrap() > 1_700_000_000);
        assert_eq!(fields[2], "Siphon");

        let raw = xattr::get(&file, "com.apple.metadata:kMDItemWhereFroms").unwrap().unwrap();
        assert!(raw.starts_with(b"bplist00"));
        let sources: Vec<String> = plist::from_bytes(&raw).unwrap();
        assert_eq!(sources, vec![origin.url.clone(), "https://example.com/download".to_owned()]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_command_receives_the_file_as_an_argument() {
        let dir = scratch("command");
        // A name that would wreck a naively quoted command line.
        let file = dir.join("it's \"odd\" $(name); rm -rf x.bin");
        std::fs::write(&file, b"payload").unwrap();
        let out = dir.join("out.txt");
        let origin = Origin { url: "https://a.example/x".into(), ..Default::default() };

        let command = format!("cat [File] > '{}'; printf '%s' \"$SIPHON_URL\" >> '{}'", out.display(), out.display());
        run_command(&command, &file, &origin).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::fs::read(&out).map(|b| b.len()).unwrap_or(0) < 27 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "payloadhttps://a.example/x");
        assert!(file.exists(), "nothing in the file name was executed");

        // Without the placeholder the file is appended as the last argument.
        let copy = dir.join("copy.bin");
        run_command(&format!("cp -f \"$1\" '{}' #", copy.display()), &file, &origin).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !copy.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(std::fs::read(&copy).unwrap(), b"payload");
        run_command("   ", &file, &origin).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
