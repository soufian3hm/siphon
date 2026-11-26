//! Native-messaging host.
//!
//! The browser starts this program and talks to it over stdin and stdout
//! with length-prefixed JSON. It does no downloading itself: it forwards
//! what the extension captured to the running application over the local
//! endpoint, starting the application first if it is not running.
//!
//! Wire format, both directions: a 32-bit length in native byte order
//! followed by that many bytes of UTF-8 JSON.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};
use siphon_core::Capture;
use siphon_core::grab::LinkBatch;
use siphon_core::ipc::{self, Message, SendError};

/// The browser never sends more than this to a host.
const MAX_INCOMING: usize = 64 * 1024 * 1024;
/// And accepts no more than this from one.
const MAX_OUTGOING: usize = 1024 * 1024;
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Deserialize)]
struct Incoming {
    /// Echoed back so the extension can pair replies with requests.
    #[serde(default)]
    id: Value,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    capture: Option<Capture>,
    /// The links of a page, for the application's link picker.
    #[serde(default)]
    links: Option<LinkBatch>,
}

fn read_message(input: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match input.read_exact(&mut len) {
        Ok(()) => {}
        // The browser closed the pipe: the extension is gone, so are we.
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_ne_bytes(len) as usize;
    if len > MAX_INCOMING {
        return Err(io::Error::other("message exceeds the native messaging limit"));
    }
    let mut body = vec![0u8; len];
    input.read_exact(&mut body)?;
    Ok(Some(body))
}

fn write_message(output: &mut impl Write, value: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(value)?;
    if body.len() > MAX_OUTGOING {
        return Err(io::Error::other("reply exceeds the native messaging limit"));
    }
    output.write_all(&(body.len() as u32).to_ne_bytes())?;
    output.write_all(&body)?;
    output.flush()
}

fn data_dir() -> PathBuf {
    std::env::var_os("SIPHON_DATA_DIR").map(PathBuf::from).unwrap_or_else(siphon_core::settings::default_data_dir)
}

/// The application that ships next to this host.
fn application() -> Option<PathBuf> {
    let me = std::env::current_exe().ok()?.canonicalize().ok()?;
    let dir = me.parent()?;
    let name = if cfg!(windows) { "siphon.exe" } else { "siphon" };
    let sibling = dir.join(name);
    sibling.is_file().then_some(sibling)
}

/// On macOS the application is a bundle; launching the bundle (rather than
/// the binary inside it) gives it a Dock icon and normal window behaviour.
fn bundle_of(binary: &Path) -> Option<PathBuf> {
    siphon_core::browser::bundle_of(binary)
}

/// Brings the application to the front after handing it something the user
/// is meant to look at. Done from here as well as by the application itself:
/// this helper runs under the browser the user is working in, which is the
/// party the system lets hand the foreground to someone else.
fn show(reply: &ipc::Reply) {
    if reply.attention && reply.pid != 0 {
        let bundle = application().and_then(|binary| bundle_of(&binary));
        siphon_core::browser::bring_forward(reply.pid, bundle.as_deref());
    }
}

fn launch() -> Result<(), String> {
    let binary = application().ok_or("Siphon is not installed next to its browser helper")?;
    let mut command = match bundle_of(&binary).filter(|_| cfg!(target_os = "macos")) {
        Some(bundle) => {
            let mut open = std::process::Command::new("/usr/bin/open");
            open.arg(bundle);
            open
        }
        None => std::process::Command::new(&binary),
    };
    // The browser owns our stdin and stdout; the application must not
    // inherit them or it would corrupt the message stream.
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not start Siphon: {e}"))
}

/// Delivers a message, starting the application when it is not running.
fn deliver(message: &Message, start_if_needed: bool) -> Result<String, String> {
    let dir = data_dir();
    match ipc::send(&dir, message) {
        Ok(reply) => {
            show(&reply);
            return Ok(reply.version.unwrap_or_default());
        }
        Err(SendError::NotRunning) if start_if_needed => {}
        Err(e) => return Err(e.to_string()),
    }

    launch()?;
    let deadline = Instant::now() + LAUNCH_TIMEOUT;
    loop {
        std::thread::sleep(Duration::from_millis(150));
        match ipc::send(&dir, message) {
            Ok(reply) => return Ok(reply.version.unwrap_or_default()),
            Err(SendError::NotRunning) if Instant::now() < deadline => {}
            Err(SendError::NotRunning) => return Err("Siphon did not start in time".into()),
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn handle(incoming: Incoming) -> Value {
    let id = incoming.id;
    let result = match incoming.kind.as_str() {
        // "Is the application there?" must not start it: the extension asks
        // on every browser launch.
        "ping" => {
            return match deliver(&Message::Ping, false) {
                Ok(version) => json!({ "id": id, "ok": true, "running": true, "version": version }),
                Err(_) => json!({
                    "id": id,
                    "ok": true,
                    "running": false,
                    "installed": application().is_some(),
                }),
            };
        }
        "capture" => match incoming.capture {
            Some(capture) if !capture.request.url.trim().is_empty() => deliver(&Message::Capture { capture }, true),
            _ => Err("the capture has no address".into()),
        },
        "links" => match incoming.links {
            Some(batch) if !batch.links.is_empty() => deliver(&Message::Links { batch }, true),
            _ => Err("there are no links to hand over".into()),
        },
        "raise" => deliver(&Message::Raise, true),
        other => Err(format!("unknown request \"{other}\"")),
    };
    match result {
        Ok(version) => json!({ "id": id, "ok": true, "version": version }),
        Err(error) => json!({ "id": id, "ok": false, "error": error }),
    }
}

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();

    // A closed pipe or a malformed frame both mean the browser is done with us.
    while let Ok(Some(body)) = read_message(&mut input) {
        let reply = match serde_json::from_slice::<Incoming>(&body) {
            Ok(incoming) => handle(incoming),
            Err(e) => json!({ "ok": false, "error": format!("malformed request: {e}") }),
        };
        if write_message(&mut output, &reply).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_round_trips() {
        let mut wire = Vec::new();
        write_message(&mut wire, &json!({ "type": "ping", "id": 7 })).unwrap();
        let first_len = u32::from_ne_bytes(wire[..4].try_into().unwrap()) as usize;
        assert_eq!(wire.len(), 4 + first_len, "the prefix is the byte length of the JSON that follows");
        write_message(&mut wire, &json!({ "type": "raise" })).unwrap();

        let mut reader = &wire[..];
        let first: Value = serde_json::from_slice(&read_message(&mut reader).unwrap().unwrap()).unwrap();
        assert_eq!(first, json!({ "type": "ping", "id": 7 }));
        let second: Value = serde_json::from_slice(&read_message(&mut reader).unwrap().unwrap()).unwrap();
        assert_eq!(second["type"], "raise");
        assert!(read_message(&mut reader).unwrap().is_none(), "end of pipe is a clean exit");
    }

    #[test]
    fn oversized_and_truncated_messages_are_rejected() {
        let mut huge = ((MAX_INCOMING + 1) as u32).to_ne_bytes().to_vec();
        huge.extend_from_slice(b"{}");
        assert!(read_message(&mut &huge[..]).is_err());

        let mut short = 10u32.to_ne_bytes().to_vec();
        short.extend_from_slice(b"{}");
        assert!(read_message(&mut &short[..]).is_err());
    }

    #[test]
    fn captures_parse_from_the_extension_format() {
        let incoming: Incoming = serde_json::from_value(json!({
            "id": "c1",
            "type": "capture",
            "capture": {
                "request": {
                    "url": "https://cdn.example/a.zip",
                    "referer": "https://example.com/downloads",
                    "headers": [["Cookie", "sid=1"], ["User-Agent", "Chrome"]]
                },
                "suggested_name": "a.zip",
                "size": 1048576,
                "mime": "application/zip",
                "page_title": "Downloads",
                "media": false
            }
        }))
        .unwrap();
        let capture = incoming.capture.unwrap();
        assert_eq!(capture.request.url, "https://cdn.example/a.zip");
        assert_eq!(capture.request.header("cookie"), Some("sid=1"));
        assert_eq!(capture.size, Some(1_048_576));

        // Everything but the address is optional.
        let minimal: Incoming =
            serde_json::from_value(json!({ "type": "capture", "capture": { "request": { "url": "https://x.example/f" } } })).unwrap();
        assert_eq!(minimal.capture.unwrap().suggested_name, None);
    }

    #[test]
    fn bundle_detection() {
        assert_eq!(
            bundle_of(Path::new("/Applications/Siphon.app/Contents/MacOS/siphon")),
            Some(PathBuf::from("/Applications/Siphon.app"))
        );
        assert_eq!(bundle_of(Path::new("/usr/local/bin/siphon")), None);
    }

    #[test]
    fn unknown_requests_are_answered_not_fatal() {
        let reply = handle(Incoming { id: json!(3), kind: "dance".into(), capture: None, links: None });
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["id"], 3);
        let reply = handle(Incoming { id: Value::Null, kind: "capture".into(), capture: None, links: None });
        assert_eq!(reply["ok"], false);
        let reply = handle(Incoming { id: Value::Null, kind: "links".into(), capture: None, links: Some(LinkBatch::default()) });
        assert_eq!(reply["ok"], false);
    }

    #[test]
    fn link_batches_parse_from_the_extension_format() {
        let incoming: Incoming = serde_json::from_value(json!({
            "id": 9,
            "type": "links",
            "links": {
                "page_url": "https://example.com/gallery/",
                "page_title": "Gallery",
                "headers": [["User-Agent", "Chrome"], ["Cookie", "sid=1"]],
                "links": [
                    { "url": "https://example.com/a.zip", "text": "Archive", "kind": "file" },
                    { "url": "https://example.com/about", "text": "", "kind": "page" },
                    { "url": "https://cdn.example.net/p.jpg", "text": "A photo", "kind": "embedded" }
                ]
            }
        }))
        .unwrap();
        let batch = incoming.links.unwrap();
        assert_eq!(batch.links.len(), 3);
        assert_eq!(batch.links[0].kind, siphon_core::grab::LinkKind::File);
        assert_eq!(batch.links[2].kind, siphon_core::grab::LinkKind::Embedded);
        assert_eq!(batch.request_for("https://example.com/a.zip").header("cookie"), Some("sid=1"));
        assert_eq!(batch.request_for("https://cdn.example.net/p.jpg").header("cookie"), None);
    }
}
