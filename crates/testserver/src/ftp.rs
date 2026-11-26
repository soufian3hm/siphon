//! A minimal passive-mode FTP server: enough of RFC 959 and RFC 3659 for a
//! client to log in, ask for a size, restart at an offset and retrieve.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::Content;

#[derive(Clone, Debug)]
pub struct FtpOptions {
    /// Required credentials; `None` accepts anyone.
    pub login: Option<(String, String)>,
    /// Answer `SIZE`.
    pub size: bool,
    /// Honour `REST`.
    pub rest: bool,
    /// Bytes per second on each data connection.
    pub throttle: Option<u64>,
    /// Sessions allowed at once; the rest are greeted with `421`.
    pub max_sessions: Option<usize>,
}

impl Default for FtpOptions {
    fn default() -> Self {
        Self { login: None, size: true, rest: true, throttle: None, max_sessions: None }
    }
}

#[derive(Clone, Debug, Default)]
pub struct FtpStats {
    pub sessions: usize,
    pub peak_sessions: usize,
    /// Every command received, in order, across all sessions.
    pub commands: Vec<String>,
    /// The `REST` offset of every `RETR`.
    pub retr_offsets: Vec<u64>,
    pub bytes_sent: u64,
}

struct State {
    files: Mutex<HashMap<String, Content>>,
    options: Mutex<FtpOptions>,
    stats: Mutex<FtpStats>,
    live: AtomicUsize,
}

pub struct FtpTestServer {
    addr: SocketAddr,
    state: Arc<State>,
    accept: tokio::task::JoinHandle<()>,
}

impl Drop for FtpTestServer {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl FtpTestServer {
    pub async fn start(options: FtpOptions) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(State {
            files: Mutex::new(HashMap::new()),
            options: Mutex::new(options),
            stats: Mutex::new(FtpStats::default()),
            live: AtomicUsize::new(0),
        });
        let accept = tokio::spawn({
            let state = state.clone();
            async move {
                loop {
                    let Ok((socket, _)) = listener.accept().await else { break };
                    tokio::spawn(session(socket, state.clone()));
                }
            }
        });
        Self { addr, state, accept }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn url(&self, path: &str) -> String {
        format!("ftp://{}{}", self.addr, path)
    }

    pub fn url_with_login(&self, user: &str, password: &str, path: &str) -> String {
        format!("ftp://{user}:{password}@{}{}", self.addr, path)
    }

    pub fn file(&self, path: &str, content: Content) {
        self.state.files.lock().insert(path.to_owned(), content);
    }

    pub fn remove(&self, path: &str) {
        self.state.files.lock().remove(path);
    }

    pub fn stats(&self) -> FtpStats {
        self.state.stats.lock().clone()
    }
}

struct Live<'a>(&'a State);
impl Drop for Live<'_> {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn session(socket: TcpStream, state: Arc<State>) {
    let live = state.live.fetch_add(1, Ordering::SeqCst) + 1;
    let _guard = Live(&state);
    {
        let mut stats = state.stats.lock();
        stats.sessions += 1;
        stats.peak_sessions = stats.peak_sessions.max(live);
    }
    let options = state.options.lock().clone();
    let (read, mut write) = socket.into_split();
    let mut lines = BufReader::new(read).lines();

    macro_rules! reply {
        ($($arg:tt)*) => {
            if write.write_all(format!("{}\r\n", format!($($arg)*)).as_bytes()).await.is_err() {
                return;
            }
        };
    }

    if options.max_sessions.is_some_and(|max| live > max) {
        reply!("421 Too many connections from your address");
        return;
    }
    reply!("220 siphon test ftp");

    let mut user = String::new();
    let mut authed = options.login.is_none();
    let mut passive: Option<TcpListener> = None;
    let mut rest = 0u64;

    while let Ok(Some(line)) = lines.next_line().await {
        let (verb, arg) = match line.split_once(' ') {
            Some((v, a)) => (v.to_ascii_uppercase(), a.trim().to_owned()),
            None => (line.trim().to_ascii_uppercase(), String::new()),
        };
        state.stats.lock().commands.push(if verb == "PASS" { "PASS ***".into() } else { line.clone() });

        match verb.as_str() {
            "USER" => {
                user = arg;
                reply!("331 Password required");
            }
            "PASS" => match &options.login {
                Some((u, p)) if *u != user || *p != arg => reply!("530 Login incorrect"),
                _ => {
                    authed = true;
                    reply!("230 Logged in");
                }
            },
            "QUIT" => {
                reply!("221 Bye");
                return;
            }
            _ if !authed => reply!("530 Please log in"),
            "SYST" => reply!("215 UNIX Type: L8"),
            "FEAT" => {
                let mut features = String::from("211-Features:\r\n");
                if options.size {
                    features.push_str(" SIZE\r\n");
                }
                features.push_str(" MDTM\r\n");
                if options.rest {
                    features.push_str(" REST STREAM\r\n");
                }
                features.push_str("211 End");
                reply!("{features}");
            }
            "OPTS" | "NOOP" | "MODE" | "STRU" => reply!("200 OK"),
            "TYPE" => reply!("200 Type set to {arg}"),
            "PWD" => reply!("257 \"/\" is the current directory"),
            "CWD" => reply!("250 OK"),
            "PASV" => match TcpListener::bind("127.0.0.1:0").await {
                Ok(listener) => {
                    let port = listener.local_addr().unwrap().port();
                    passive = Some(listener);
                    reply!("227 Entering Passive Mode (127,0,0,1,{},{})", port >> 8, port & 0xFF);
                }
                Err(_) => reply!("425 Cannot open data connection"),
            },
            "EPSV" => match TcpListener::bind("127.0.0.1:0").await {
                Ok(listener) => {
                    let port = listener.local_addr().unwrap().port();
                    passive = Some(listener);
                    reply!("229 Entering Extended Passive Mode (|||{port}|)");
                }
                Err(_) => reply!("425 Cannot open data connection"),
            },
            "SIZE" => {
                let content = state.files.lock().get(&arg).cloned();
                match content {
                    Some(c) if options.size => reply!("213 {}", c.len()),
                    Some(_) => reply!("502 SIZE not implemented"),
                    None => reply!("550 {arg}: No such file"),
                }
            }
            "MDTM" => {
                if state.files.lock().contains_key(&arg) {
                    reply!("213 20261126101500");
                } else {
                    reply!("550 {arg}: No such file");
                }
            }
            "REST" => match arg.parse::<u64>() {
                Ok(offset) if options.rest => {
                    rest = offset;
                    reply!("350 Restarting at {offset}");
                }
                Ok(_) => reply!("502 REST not implemented"),
                Err(_) => reply!("501 Bad offset"),
            },
            "RETR" => {
                let offset = std::mem::take(&mut rest);
                let content = state.files.lock().get(&arg).cloned();
                let Some(content) = content else {
                    passive = None;
                    reply!("550 {arg}: No such file");
                    continue;
                };
                let Some(listener) = passive.take() else {
                    reply!("425 Use PASV first");
                    continue;
                };
                state.stats.lock().retr_offsets.push(offset);
                reply!("150 Opening BINARY mode data connection");
                let accepted = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await;
                let Ok(Ok((mut data, _))) = accepted else {
                    reply!("425 Cannot open data connection");
                    continue;
                };
                let complete = send_file(&mut data, &content, offset, options.throttle, &state).await;
                drop(data);
                if complete {
                    reply!("226 Transfer complete");
                } else {
                    reply!("426 Connection closed; transfer aborted");
                }
            }
            "ABOR" => reply!("226 Abort successful"),
            _ => reply!("502 Command not implemented"),
        }
    }
}

async fn send_file(data: &mut TcpStream, content: &Content, offset: u64, throttle: Option<u64>, state: &State) -> bool {
    let len = content.len();
    let piece = match throttle {
        Some(rate) => (rate / 20).clamp(256, 64 * 1024) as usize,
        None => 64 * 1024,
    };
    let mut buf = vec![0u8; piece];
    let mut at = offset.min(len);
    let started = tokio::time::Instant::now();
    while at < len {
        let n = (len - at).min(piece as u64) as usize;
        content.read(at, &mut buf[..n]);
        if data.write_all(&buf[..n]).await.is_err() {
            return false;
        }
        at += n as u64;
        state.stats.lock().bytes_sent += n as u64;
        if let Some(rate) = throttle {
            let due = started + Duration::from_secs_f64((at - offset) as f64 / rate as f64);
            tokio::time::sleep_until(due).await;
        }
    }
    data.shutdown().await.is_ok()
}
