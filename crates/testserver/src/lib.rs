//! A small HTTP/1.1 server that can be told to misbehave in every way a real
//! one does: ignore ranges, throttle, cap connections, expire links, drop
//! connections mid-body, change the file underneath a download.
//!
//! Written by hand on raw sockets so that tests control every byte on the
//! wire. Content is generated from the offset, so multi-gigabyte files cost
//! no memory and any range can be verified independently.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub mod ftp;
pub use ftp::{FtpOptions, FtpTestServer};

/// Deterministic file content: the byte at any offset depends only on the
/// seed and the offset.
pub fn fill(seed: u64, offset: u64, buf: &mut [u8]) {
    fn word(seed: u64, index: u64) -> u64 {
        // splitmix64
        let mut z = seed ^ index.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    let mut i = 0usize;
    while i < buf.len() {
        let absolute = offset + i as u64;
        let bytes = word(seed, absolute / 8).to_le_bytes();
        let within = (absolute % 8) as usize;
        let n = (8 - within).min(buf.len() - i);
        buf[i..i + n].copy_from_slice(&bytes[within..within + n]);
        i += n;
    }
}

/// The whole content of a pattern file, for small files in tests.
pub fn pattern(seed: u64, size: u64) -> Vec<u8> {
    let mut buf = vec![0u8; size as usize];
    fill(seed, 0, &mut buf);
    buf
}

/// Checks a file on disk against the pattern, reporting the first bad offset.
pub fn verify_file(path: &std::path::Path, seed: u64, size: u64) -> Result<(), String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let len = file.metadata().map_err(|e| e.to_string())?.len();
    if len != size {
        return Err(format!("size is {len}, expected {size}"));
    }
    let mut got = vec![0u8; 1 << 20];
    let mut want = vec![0u8; 1 << 20];
    let mut offset = 0u64;
    loop {
        let n = file.read(&mut got).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        fill(seed, offset, &mut want[..n]);
        if got[..n] != want[..n] {
            let bad = got[..n].iter().zip(&want[..n]).position(|(a, b)| a != b).unwrap_or(0);
            return Err(format!("content differs at byte {}", offset + bad as u64));
        }
        offset += n as u64;
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub enum Content {
    Pattern { size: u64, seed: u64 },
    Bytes(Arc<Vec<u8>>),
}

impl Content {
    pub fn len(&self) -> u64 {
        match self {
            Content::Pattern { size, .. } => *size,
            Content::Bytes(b) => b.len() as u64,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copies `buf.len()` bytes starting at `offset` into `buf`.
    pub fn read(&self, offset: u64, buf: &mut [u8]) {
        match self {
            Content::Pattern { seed, .. } => fill(*seed, offset, buf),
            Content::Bytes(b) => buf.copy_from_slice(&b[offset as usize..offset as usize + buf.len()]),
        }
    }
}

/// Builds a ZIP archive of stored (uncompressed) entries, for routes that
/// need to serve a real archive.
pub fn stored_zip(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut central: Vec<u8> = Vec::new();
    for (name, data) in entries {
        let offset = out.len() as u32;
        let size = data.len() as u32;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&[20, 0, 0, 0x08]);
        out.extend_from_slice(&[0u8; 2 + 2 + 2 + 4]);
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&[20, 0, 20, 0, 0, 0x08]);
        central.extend_from_slice(&[0u8; 2 + 2 + 2 + 4]);
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0u8; 2 + 2 + 2 + 2 + 4]);
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let directory_offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0u8; 4]);
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&directory_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// How one path behaves.
#[derive(Clone, Debug)]
pub struct Route {
    pub content: Content,
    /// Honour `Range` requests.
    pub ranges: bool,
    /// Send `Content-Length`. When false the body is chunk-encoded and the
    /// client cannot know the size.
    pub announce_length: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: String,
    pub disposition: Option<String>,
    /// Bytes per second, per connection.
    pub throttle: Option<u64>,
    /// Simultaneous responses allowed; the rest get `503`.
    pub max_connections: Option<usize>,
    /// Required value of the `token` query parameter.
    pub token: Option<String>,
    /// Status sent when the token is missing or wrong.
    pub denied_status: u16,
    /// Answer with a `302` to this location instead of content.
    pub redirect: Option<String>,
    /// Close the socket after this many body bytes of every response.
    pub drop_after: Option<u64>,
    /// Answer `503` to this many requests before behaving.
    pub fail_first: u32,
    /// Wait before answering.
    pub delay: Option<Duration>,
    /// Extra response headers.
    pub headers: Vec<(String, String)>,
    /// Exact value the `Authorization` header must have; anything else
    /// gets a `401` asking for Basic credentials.
    pub authorization: Option<String>,
}

impl Route {
    pub fn pattern(size: u64, seed: u64) -> Self {
        Self::new(Content::Pattern { size, seed })
    }

    pub fn bytes(bytes: impl Into<Vec<u8>>, content_type: &str) -> Self {
        let mut route = Self::new(Content::Bytes(Arc::new(bytes.into())));
        route.content_type = content_type.to_owned();
        route
    }

    pub fn redirect(location: impl Into<String>) -> Self {
        let mut route = Self::new(Content::Bytes(Arc::new(Vec::new())));
        route.redirect = Some(location.into());
        route
    }

    fn new(content: Content) -> Self {
        Self {
            content,
            ranges: true,
            announce_length: true,
            etag: None,
            last_modified: None,
            content_type: "application/octet-stream".into(),
            disposition: None,
            throttle: None,
            max_connections: None,
            token: None,
            authorization: None,
            denied_status: 403,
            redirect: None,
            drop_after: None,
            fail_first: 0,
            delay: None,
            headers: Vec::new(),
        }
    }

    pub fn no_ranges(mut self) -> Self {
        self.ranges = false;
        self
    }
    pub fn no_length(mut self) -> Self {
        self.announce_length = false;
        self.ranges = false;
        self
    }
    pub fn etag(mut self, etag: &str) -> Self {
        self.etag = Some(etag.to_owned());
        self
    }
    pub fn last_modified(mut self, value: &str) -> Self {
        self.last_modified = Some(value.to_owned());
        self
    }
    pub fn content_type(mut self, value: &str) -> Self {
        self.content_type = value.to_owned();
        self
    }
    pub fn disposition(mut self, value: &str) -> Self {
        self.disposition = Some(value.to_owned());
        self
    }
    pub fn throttle(mut self, bytes_per_sec: u64) -> Self {
        self.throttle = Some(bytes_per_sec);
        self
    }
    pub fn max_connections(mut self, n: usize) -> Self {
        self.max_connections = Some(n);
        self
    }
    pub fn token(mut self, token: &str) -> Self {
        self.token = Some(token.to_owned());
        self
    }
    /// Requires HTTP Basic credentials. `encoded` is the base64 of
    /// `user:password`.
    pub fn basic_auth(mut self, encoded: &str) -> Self {
        self.authorization = Some(format!("Basic {encoded}"));
        self
    }

    pub fn denied_status(mut self, status: u16) -> Self {
        self.denied_status = status;
        self
    }
    pub fn drop_after(mut self, bytes: u64) -> Self {
        self.drop_after = Some(bytes);
        self
    }
    pub fn fail_first(mut self, n: u32) -> Self {
        self.fail_first = n;
        self
    }
    pub fn delay(mut self, d: Duration) -> Self {
        self.delay = Some(d);
        self
    }
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }
}

/// What a path has seen so far.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub requests: u64,
    /// Requests that carried a `Range` header.
    pub ranged: u64,
    /// Most responses in flight at the same moment.
    pub peak_connections: usize,
    pub bytes_sent: u64,
    pub statuses: Vec<u16>,
    /// The `Range` header of every request, in arrival order.
    pub ranges: Vec<String>,
    /// Request headers of the most recent request, lower-cased names.
    pub last_headers: HashMap<String, String>,
}

#[derive(Default)]
struct Counters {
    in_flight: AtomicUsize,
    bytes_sent: AtomicU64,
    stats: Mutex<Stats>,
}

struct Entry {
    route: Mutex<Route>,
    counters: Counters,
}

#[derive(Default)]
struct State {
    routes: Mutex<HashMap<String, Arc<Entry>>>,
}

/// A running server. Dropping it stops accepting connections.
pub struct TestServer {
    addr: SocketAddr,
    state: Arc<State>,
    accept: tokio::task::JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl TestServer {
    /// Starts on an ephemeral loopback port.
    pub async fn start() -> Self {
        Self::bind("127.0.0.1:0").await.expect("bind loopback")
    }

    pub async fn bind(addr: &str) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        let addr = listener.local_addr()?;
        let state = Arc::new(State::default());
        let accept = tokio::spawn({
            let state = state.clone();
            async move {
                loop {
                    let Ok((socket, _)) = listener.accept().await else { break };
                    let _ = socket.set_nodelay(true);
                    tokio::spawn(serve_connection(socket, state.clone()));
                }
            }
        });
        Ok(Self { addr, state, accept })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    /// Adds or replaces a path.
    pub fn route(&self, path: &str, route: Route) {
        self.state
            .routes
            .lock()
            .insert(path.to_owned(), Arc::new(Entry { route: Mutex::new(route), counters: Counters::default() }));
    }

    /// Changes how an existing path behaves, keeping its statistics.
    pub fn update(&self, path: &str, change: impl FnOnce(&mut Route)) {
        let entry = self.state.routes.lock().get(path).cloned().expect("unknown route");
        change(&mut entry.route.lock());
    }

    pub fn stats(&self, path: &str) -> Stats {
        let entry = self.state.routes.lock().get(path).cloned().expect("unknown route");
        let mut stats = entry.counters.stats.lock().clone();
        stats.bytes_sent = entry.counters.bytes_sent.load(Ordering::Relaxed);
        stats
    }
}

struct RequestHead {
    method: String,
    path: String,
    query: String,
    headers: HashMap<String, String>,
}

async fn read_head(socket: &mut TcpStream, pending: &mut Vec<u8>) -> Option<RequestHead> {
    loop {
        if let Some(end) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&pending[..end]).into_owned();
            pending.drain(..end + 4);
            let mut lines = head.split("\r\n");
            let mut request_line = lines.next()?.split(' ');
            let method = request_line.next()?.to_owned();
            let target = request_line.next()?.to_owned();
            // Proxies receive absolute-form targets.
            let target = match target.find("://") {
                Some(i) => {
                    let rest = &target[i + 3..];
                    rest.find('/').map(|j| rest[j..].to_owned()).unwrap_or_else(|| "/".into())
                }
                None => target,
            };
            let (path, query) = match target.split_once('?') {
                Some((p, q)) => (p.to_owned(), q.to_owned()),
                None => (target, String::new()),
            };
            let headers = lines
                .filter_map(|l| l.split_once(':'))
                .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
                .collect();
            return Some(RequestHead { method, path, query, headers });
        }
        if pending.len() > 64 * 1024 {
            return None;
        }
        let mut buf = [0u8; 4096];
        match socket.read(&mut buf).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => pending.extend_from_slice(&buf[..n]),
        }
    }
}

fn query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == name).map(|(_, v)| v)
}

/// Parses `bytes=a-b`, `bytes=a-` and `bytes=-n` against a file of `len` bytes.
fn parse_range(value: &str, len: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = value.trim().strip_prefix("bytes=")?;
    let (a, b) = spec.split_once('-')?;
    let range = if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        if n == 0 || len == 0 {
            return Some(Err(()));
        }
        (len.saturating_sub(n), len - 1)
    } else {
        let first: u64 = a.parse().ok()?;
        let last: u64 = if b.is_empty() { len.saturating_sub(1) } else { b.parse().ok()? };
        if first >= len || first > last {
            return Some(Err(()));
        }
        (first, last.min(len - 1))
    };
    Some(Ok(range))
}

async fn simple(socket: &mut TcpStream, status: u16, reason: &str, extra: &str, body: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n{extra}\r\n",
        body.len()
    );
    socket.write_all(head.as_bytes()).await?;
    socket.write_all(body.as_bytes()).await
}

struct InFlight<'a>(&'a Counters);
impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn serve_connection(mut socket: TcpStream, state: Arc<State>) {
    let mut pending = Vec::new();
    while let Some(request) = read_head(&mut socket, &mut pending).await {
        let entry = state.routes.lock().get(&request.path).cloned();
        let Some(entry) = entry else {
            if simple(&mut socket, 404, "Not Found", "", "no such route").await.is_err() {
                return;
            }
            continue;
        };
        match respond(&mut socket, &request, &entry).await {
            Ok(true) => {}
            Ok(false) | Err(_) => return,
        }
    }
}

/// Returns `Ok(false)` when the connection must be closed afterwards.
async fn respond(socket: &mut TcpStream, request: &RequestHead, entry: &Entry) -> std::io::Result<bool> {
    let route = entry.route.lock().clone();
    let range_header = request.headers.get("range").cloned();
    let record = |status: u16| {
        let mut stats = entry.counters.stats.lock();
        stats.requests += 1;
        stats.statuses.push(status);
        if let Some(r) = &range_header {
            stats.ranged += 1;
            stats.ranges.push(r.clone());
        }
        stats.last_headers = request.headers.clone();
    };

    if let Some(delay) = route.delay {
        tokio::time::sleep(delay).await;
    }

    if route.fail_first > 0 {
        let seen = entry.counters.stats.lock().requests;
        if seen < route.fail_first as u64 {
            record(503);
            simple(socket, 503, "Service Unavailable", "Retry-After: 0\r\n", "warming up").await?;
            return Ok(true);
        }
    }

    if let Some(expected) = &route.authorization
        && request.headers.get("authorization") != Some(expected)
    {
        record(401);
        simple(socket, 401, "Unauthorized", "WWW-Authenticate: Basic realm=\"files\"\r\n", "login required").await?;
        return Ok(true);
    }

    if let Some(token) = &route.token
        && query_param(&request.query, "token") != Some(token.as_str())
    {
        record(route.denied_status);
        simple(socket, route.denied_status, "Denied", "", "link expired").await?;
        return Ok(true);
    }

    if let Some(location) = &route.redirect {
        record(302);
        simple(socket, 302, "Found", &format!("Location: {location}\r\n"), "").await?;
        return Ok(true);
    }

    let in_flight = entry.counters.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    let _guard = InFlight(&entry.counters);
    {
        let mut stats = entry.counters.stats.lock();
        stats.peak_connections = stats.peak_connections.max(in_flight);
    }
    if route.max_connections.is_some_and(|max| in_flight > max) {
        record(503);
        simple(socket, 503, "Service Unavailable", "Retry-After: 1\r\n", "too many connections").await?;
        return Ok(true);
    }

    let len = route.content.len();
    let mut common = format!("Content-Type: {}\r\n", route.content_type);
    if let Some(etag) = &route.etag {
        common.push_str(&format!("ETag: {etag}\r\n"));
    }
    if let Some(lm) = &route.last_modified {
        common.push_str(&format!("Last-Modified: {lm}\r\n"));
    }
    if let Some(d) = &route.disposition {
        common.push_str(&format!("Content-Disposition: {d}\r\n"));
    }
    if route.ranges {
        common.push_str("Accept-Ranges: bytes\r\n");
    }
    for (name, value) in &route.headers {
        common.push_str(&format!("{name}: {value}\r\n"));
    }

    // `If-Range` with a stale validator means "send me the whole thing".
    let if_range_ok = match (request.headers.get("if-range"), &route.etag) {
        (Some(sent), Some(current)) => sent == current,
        (Some(_), None) => false,
        (None, _) => true,
    };

    let (status, first, last) = match (route.ranges && if_range_ok, range_header.as_deref()) {
        (true, Some(value)) => match parse_range(value, len) {
            Some(Ok((first, last))) => (206, first, last),
            Some(Err(())) => {
                record(416);
                simple(socket, 416, "Range Not Satisfiable", &format!("Content-Range: bytes */{len}\r\n"), "").await?;
                return Ok(true);
            }
            None => (200, 0, len.saturating_sub(1)),
        },
        _ => (200, 0, len.saturating_sub(1)),
    };
    let body_len = if len == 0 { 0 } else { last - first + 1 };
    record(status);

    let mut head = format!("HTTP/1.1 {status} {}\r\n{common}", if status == 206 { "Partial Content" } else { "OK" });
    if status == 206 {
        head.push_str(&format!("Content-Range: bytes {first}-{last}/{len}\r\n"));
    }
    let chunked = !route.announce_length;
    if chunked {
        head.push_str("Transfer-Encoding: chunked\r\n");
    } else {
        head.push_str(&format!("Content-Length: {body_len}\r\n"));
    }
    head.push_str("\r\n");
    socket.write_all(head.as_bytes()).await?;
    if request.method.eq_ignore_ascii_case("HEAD") {
        return Ok(true);
    }

    // Small pieces when throttled, so the pacing is smooth.
    let piece = match route.throttle {
        Some(rate) => ((rate / 20).clamp(256, 64 * 1024)) as usize,
        None => 64 * 1024,
    };
    let mut buf = vec![0u8; piece];
    let mut sent = 0u64;
    let started = tokio::time::Instant::now();
    while sent < body_len {
        let n = (body_len - sent).min(piece as u64) as usize;
        if let Some(limit) = route.drop_after
            && sent >= limit
        {
            // Mid-body disconnect.
            return Ok(false);
        }
        route.content.read(first + sent, &mut buf[..n]);
        if chunked {
            socket.write_all(format!("{n:x}\r\n").as_bytes()).await?;
            socket.write_all(&buf[..n]).await?;
            socket.write_all(b"\r\n").await?;
        } else {
            socket.write_all(&buf[..n]).await?;
        }
        sent += n as u64;
        entry.counters.bytes_sent.fetch_add(n as u64, Ordering::Relaxed);
        if let Some(rate) = route.throttle {
            let due = started + Duration::from_secs_f64(sent as f64 / rate as f64);
            tokio::time::sleep_until(due).await;
        }
    }
    if chunked {
        socket.write_all(b"0\r\n\r\n").await?;
    }
    socket.flush().await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_is_offset_addressable() {
        let whole = pattern(7, 10_000);
        for (offset, len) in [(0usize, 1usize), (1, 7), (5, 100), (4093, 4099), (9_999, 1)] {
            let mut piece = vec![0u8; len];
            fill(7, offset as u64, &mut piece);
            assert_eq!(piece, whole[offset..offset + len], "offset {offset}");
        }
        assert_ne!(pattern(7, 64), pattern(8, 64));
    }

    #[test]
    fn range_parsing() {
        assert_eq!(parse_range("bytes=0-0", 100), Some(Ok((0, 0))));
        assert_eq!(parse_range("bytes=10-", 100), Some(Ok((10, 99))));
        assert_eq!(parse_range("bytes=90-200", 100), Some(Ok((90, 99))));
        assert_eq!(parse_range("bytes=-10", 100), Some(Ok((90, 99))));
        assert_eq!(parse_range("bytes=100-", 100), Some(Err(())));
        assert_eq!(parse_range("bytes=0-0", 0), Some(Err(())));
        assert_eq!(parse_range("items=0-1", 100), None);
    }
}
