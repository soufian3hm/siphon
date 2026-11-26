//! The local endpoint other processes use to reach the running application:
//! the browser's native-messaging host, and a second launch of the app
//! itself.
//!
//! It is a loopback TCP socket speaking newline-delimited JSON. The port is
//! chosen by the OS and written, together with a random token, to a file in
//! the data directory that only the user can read. A peer must present the
//! token with every message, so another local user (or a web page poking at
//! localhost ports) cannot inject downloads.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::disk::write_atomic;
use crate::grab::LinkBatch;
use crate::manager::{Capture, Manager, NewDownload};
use crate::types::Request;

pub const ENDPOINT_FILE: &str = "endpoint.json";
/// Longest accepted message. A page with thousands of links fits.
const MAX_LINE: usize = 16 * 1024 * 1024;

/// Where the running application listens.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub port: u16,
    pub token: String,
    pub pid: u32,
    pub version: String,
}

impl Endpoint {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(ENDPOINT_FILE)
    }

    pub fn load(data_dir: &Path) -> Option<Self> {
        serde_json::from_slice(&std::fs::read(Self::path(data_dir)).ok()?).ok()
    }

    fn save(&self, data_dir: &Path) -> std::io::Result<()> {
        let path = Self::path(data_dir);
        write_atomic(&path, &serde_json::to_vec(self).map_err(std::io::Error::other)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}

/// A message to the application.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// Are you there?
    Ping,
    /// A link captured in the browser.
    Capture { capture: Capture },
    /// The links of a page, for the user to choose from.
    Links { batch: LinkBatch },
    /// A download requested from the command line.
    Add {
        request: Request,
        #[serde(default)]
        dir: Option<PathBuf>,
        #[serde(default)]
        name: Option<String>,
        /// Set it aside for the queue instead of starting it.
        #[serde(default)]
        later: bool,
        /// Show the confirmation dialog instead of adding it silently.
        #[serde(default)]
        ask: bool,
    },
    /// Start (`true`) or stop the queue of deferred downloads.
    Queue { start: bool },
    /// Bring the window forward.
    Raise,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Envelope {
    token: String,
    #[serde(flatten)]
    message: Message,
}

/// The application's answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The request wants the user's eyes: the sender, if it is in a better
    /// position to do so than the application, should bring it forward.
    #[serde(default)]
    pub attention: bool,
    /// The application's process, for that purpose.
    #[serde(default)]
    pub pid: u32,
}

impl Reply {
    fn ok(attention: bool) -> Self {
        Self { ok: true, error: None, version: Some(env!("CARGO_PKG_VERSION").into()), attention, pid: std::process::id() }
    }

    fn error(message: impl Into<String>) -> Self {
        Self { ok: false, error: Some(message.into()), version: None, attention: false, pid: 0 }
    }
}

/// 128 bits from the OS, hex-encoded.
fn random_token() -> std::io::Result<String> {
    let mut bytes = [0u8; 16];
    #[cfg(unix)]
    {
        use std::io::Read;
        std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    }
    #[cfg(not(unix))]
    {
        // `RandomState` is seeded from the system RNG once per process.
        use std::hash::{BuildHasher, Hasher};
        for chunk in bytes.chunks_mut(8) {
            let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
            hasher.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
            chunk.copy_from_slice(&hasher.finish().to_le_bytes()[..chunk.len()]);
        }
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Starts listening and publishes the endpoint file. The listener lives on
/// the manager's runtime and stops with it.
pub fn serve(manager: &std::sync::Arc<Manager>) -> std::io::Result<Endpoint> {
    let data_dir = manager.data_dir().to_owned();
    let runtime = manager.runtime().clone();
    let listener = {
        let _enter = runtime.enter();
        let std_listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        std_listener.set_nonblocking(true)?;
        TcpListener::from_std(std_listener)?
    };
    let endpoint = Endpoint {
        port: listener.local_addr()?.port(),
        token: random_token()?,
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").into(),
    };
    endpoint.save(&data_dir)?;

    let token = endpoint.token.clone();
    let manager = std::sync::Arc::downgrade(manager);
    runtime.spawn(async move {
        loop {
            let Ok((socket, peer)) = listener.accept().await else { break };
            if !peer.ip().is_loopback() {
                continue;
            }
            let token = token.clone();
            let manager = manager.clone();
            tokio::spawn(async move {
                let (read, mut write) = socket.into_split();
                let mut lines = tokio::io::BufReader::new(read).lines();
                // One slow or silent peer must not hold a task forever.
                while let Ok(Ok(Some(line))) = tokio::time::timeout(Duration::from_secs(120), lines.next_line()).await {
                    let reply = if line.len() > MAX_LINE {
                        Reply::error("message too large")
                    } else {
                        match serde_json::from_str::<Envelope>(&line) {
                            Err(e) => Reply::error(format!("malformed message: {e}")),
                            Ok(envelope) if !constant_time_eq(envelope.token.as_bytes(), token.as_bytes()) => {
                                Reply::error("not authorised")
                            }
                            Ok(envelope) => match manager.upgrade() {
                                None => Reply::error("the application is shutting down"),
                                Some(manager) => {
                                    let settings = manager.settings();
                                    let asks = !settings.save_automatically && settings.focus_on_capture;
                                    let attention = match &envelope.message {
                                        Message::Raise | Message::Links { .. } => true,
                                        Message::Capture { .. } => asks,
                                        Message::Add { ask, .. } => *ask && asks,
                                        Message::Ping | Message::Queue { .. } => false,
                                    };
                                    dispatch(&manager, envelope.message);
                                    Reply::ok(attention)
                                }
                            },
                        }
                    };
                    let mut out = serde_json::to_vec(&reply).unwrap_or_default();
                    out.push(b'\n');
                    if write.write_all(&out).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    Ok(endpoint)
}

/// Carries out a message. The socket uses this, and so does a launch that
/// found no running application and became it.
pub fn dispatch(manager: &Manager, message: Message) {
    match message {
        Message::Ping => {}
        Message::Capture { capture } => manager.capture(capture),
        Message::Links { batch } => manager.offer_links(batch),
        Message::Add { request, name, ask: true, .. } => manager.capture(Capture { request, suggested_name: name, ..Default::default() }),
        Message::Add { request, dir, name, later, ask: false } => {
            manager.add(NewDownload { request, dir, file_name: name, later, ..Default::default() });
        }
        Message::Queue { start: true } => {
            manager.start_queue();
        }
        Message::Queue { start: false } => {
            manager.stop_queue();
        }
        Message::Raise => manager.raise(),
    }
}

/// Removes the endpoint file if it still describes this process.
pub fn retire(data_dir: &Path) {
    if Endpoint::load(data_dir).is_some_and(|e| e.pid == std::process::id()) {
        let _ = std::fs::remove_file(Endpoint::path(data_dir));
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Why a message could not be delivered.
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("Siphon is not running")]
    NotRunning,
    #[error("could not talk to Siphon: {0}")]
    Io(#[from] std::io::Error),
    #[error("Siphon refused the request: {0}")]
    Refused(String),
}

/// Sends one message to the running application and waits for its answer.
/// Blocking and dependency-free on purpose: this is what the small
/// native-messaging host and the single-instance check use.
pub fn send(data_dir: &Path, message: &Message) -> Result<Reply, SendError> {
    let endpoint = Endpoint::load(data_dir).ok_or(SendError::NotRunning)?;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, endpoint.port));
    let mut stream = match TcpStream::connect_timeout(&addr, Duration::from_millis(800)) {
        Ok(stream) => stream,
        // A stale file from a run that did not exit cleanly.
        Err(_) => return Err(SendError::NotRunning),
    };
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.set_nodelay(true)?;

    let mut line = serde_json::to_vec(&Envelope { token: endpoint.token, message: message.clone() })
        .map_err(std::io::Error::other)?;
    line.push(b'\n');
    stream.write_all(&line)?;

    let mut answer = String::new();
    BufReader::new(&stream).read_line(&mut answer)?;
    let reply: Reply = serde_json::from_str(answer.trim()).map_err(std::io::Error::other)?;
    if reply.ok { Ok(reply) } else { Err(SendError::Refused(reply.error.unwrap_or_default())) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::Event;
    use crate::types::Request;
    use std::sync::Arc;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("siphon-ipc-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn messages_reach_the_manager_and_the_token_is_enforced() {
        let dir = scratch("roundtrip");
        assert!(matches!(send(&dir, &Message::Ping), Err(SendError::NotRunning)));

        let manager = Arc::new(Manager::open_with(&dir, crate::secrets::Secrets::in_memory(), || {}).unwrap());
        let endpoint = serve(&manager).unwrap();
        assert_eq!(endpoint.token.len(), 32);
        assert_eq!(Endpoint::load(&dir).unwrap(), endpoint);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(Endpoint::path(&dir)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the token file must be private");
        }

        let reply = send(&dir, &Message::Ping).unwrap();
        assert_eq!(reply.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));

        let capture = Capture {
            request: Request::new("http://127.0.0.1:9/a.zip").with_referer("https://files.example/"),
            suggested_name: Some("a.zip".into()),
            size: Some(123),
            ..Default::default()
        };
        send(&dir, &Message::Capture { capture: capture.clone() }).unwrap();
        send(&dir, &Message::Raise).unwrap();
        let events = manager.drain_events();
        assert!(matches!(&events[..], [Event::Captured { capture: c, .. }, Event::Raise] if c == &capture), "{events:?}");

        // The link picker's input, the command line's requests and the queue.
        let batch = LinkBatch {
            page_url: "https://files.example/list".into(),
            links: vec![crate::grab::FoundLink { url: "https://files.example/b.zip".into(), text: "B".into(), kind: crate::grab::LinkKind::File }],
            ..Default::default()
        };
        send(&dir, &Message::Links { batch: batch.clone() }).unwrap();
        assert!(matches!(&manager.drain_events()[..], [Event::Links(b)] if b == &batch));

        // Unreachable on purpose: these tests are about the list, not the network.
        let later = Request::new("http://127.0.0.1:9/later.bin");
        send(&dir, &Message::Add { request: later.clone(), dir: Some(dir.join("out")), name: Some("renamed.bin".into()), later: true, ask: false }).unwrap();
        let listed = manager.snapshot();
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].file_name.as_str(), listed[0].dir.clone(), listed[0].status.clone()), ("renamed.bin", dir.join("out"), crate::types::Status::Scheduled));
        send(&dir, &Message::Add { request: Request::new("ftp://127.0.0.1:9/asked.bin"), dir: None, name: None, later: false, ask: true }).unwrap();
        assert!(matches!(&manager.drain_events()[..], [Event::Captured { capture: c, .. }] if c.request.url == "ftp://127.0.0.1:9/asked.bin"));

        send(&dir, &Message::Queue { start: true }).unwrap();
        assert!(manager.queue().running);
        send(&dir, &Message::Queue { start: false }).unwrap();
        assert!(!manager.queue().running);
        manager.drain_events();

        // A peer without the token gets nothing done.
        let mut forged = endpoint.clone();
        forged.token = "0".repeat(32);
        forged.save(&dir).unwrap();
        assert!(matches!(send(&dir, &Message::Raise), Err(SendError::Refused(m)) if m == "not authorised"));
        assert!(manager.drain_events().is_empty());

        // Garbage is answered, not fatal.
        endpoint.save(&dir).unwrap();
        let mut raw = TcpStream::connect((Ipv4Addr::LOCALHOST, endpoint.port)).unwrap();
        raw.write_all(b"{not json}\n").unwrap();
        let mut answer = String::new();
        BufReader::new(&raw).read_line(&mut answer).unwrap();
        assert!(answer.contains("malformed"));
        send(&dir, &Message::Ping).unwrap();

        retire(&dir);
        assert!(Endpoint::load(&dir).is_none());
        drop(manager);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_stale_endpoint_file_means_not_running() {
        let dir = scratch("stale");
        std::fs::create_dir_all(&dir).unwrap();
        // A port nothing listens on. A reserved low port, not one the system
        // just handed out: with tests running side by side, a port that was
        // free a moment ago may already belong to another test's server.
        Endpoint { port: 9, token: "t".into(), pid: 1, version: "0".into() }.save(&dir).unwrap();
        assert!(matches!(send(&dir, &Message::Ping), Err(SendError::NotRunning)));
        // `retire` only removes a file this process wrote.
        retire(&dir);
        assert!(Endpoint::load(&dir).is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
