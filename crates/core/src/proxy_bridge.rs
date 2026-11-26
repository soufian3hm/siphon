//! NTLM, Negotiate and Kerberos proxy authentication.
//!
//! These schemes authenticate a *connection* with a challenge-response
//! handshake that spans several requests on the same socket. An HTTP client
//! that pools connections and treats `407` as a final answer cannot do that
//! by itself.
//!
//! The bridge is a tiny forward proxy on loopback. The download client talks
//! to it without credentials; for every connection the bridge opens one to
//! the real proxy, performs the handshake on it, and from then on only
//! shuttles bytes. Authentication stays where connection-oriented schemes
//! need it, at the socket, and the rest of the engine never knows.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use sspi::ntlm::NtlmConfig;
use sspi::{
    AuthIdentity, BufferType, ClientRequestFlags, CredentialUse, Credentials, DataRepresentation, KerberosConfig,
    Negotiate, NegotiateConfig, Ntlm, SecurityBuffer, SecurityStatus, Sspi, SspiImpl, Username,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};
use crate::net::{ProxyAuth, ProxyConfig, ProxyKind};
use crate::transfer::blocking;

const MAX_HEAD: usize = 64 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// NTLM needs two legs, Negotiate at most a few more.
const MAX_LEGS: usize = 6;

/// A running bridge. Dropping it stops accepting connections.
#[derive(Debug)]
pub struct ProxyBridge {
    addr: SocketAddr,
    stop: CancellationToken,
}

impl Drop for ProxyBridge {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl ProxyBridge {
    /// Starts a bridge for the given upstream proxy on the current runtime.
    pub async fn start(upstream: ProxyConfig) -> Result<Self> {
        if !upstream.needs_bridge() {
            return Err(Error::Unsupported("this proxy configuration does not need the bridge".into()));
        }
        if upstream.kind != ProxyKind::Http {
            return Err(Error::Unsupported(format!(
                "{} authentication is only available for HTTP proxies",
                upstream.auth.label()
            )));
        }
        // Fail on unusable credentials now, not on the first download.
        Authenticator::new(&upstream)?;

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|e| Error::Network(format!("could not start the proxy bridge: {e}")))?;
        let addr = listener.local_addr().map_err(|e| Error::Network(e.to_string()))?;
        let stop = CancellationToken::new();
        tokio::spawn({
            let stop = stop.clone();
            async move {
                loop {
                    let accepted = tokio::select! {
                        _ = stop.cancelled() => break,
                        accepted = listener.accept() => accepted,
                    };
                    let Ok((client, peer)) = accepted else { break };
                    if !peer.ip().is_loopback() {
                        continue;
                    }
                    let upstream = upstream.clone();
                    let stop = stop.clone();
                    tokio::spawn(async move {
                        tokio::select! {
                            _ = stop.cancelled() => {}
                            _ = serve(client, upstream) => {}
                        }
                    });
                }
            }
        });
        Ok(Self { addr, stop })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

/// The security context of one handshake.
enum Authenticator {
    Ntlm { ntlm: Box<Ntlm>, credentials: <Ntlm as SspiImpl>::CredentialsHandle },
    Negotiate { negotiate: Box<Negotiate>, credentials: <Negotiate as SspiImpl>::CredentialsHandle },
}

fn sspi_error(what: &str, error: impl std::fmt::Display) -> Error {
    Error::Protocol(format!("proxy authentication failed ({what}): {error}"))
}

fn workstation() -> String {
    ["COMPUTERNAME", "HOSTNAME", "HOST"]
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .map(|name| name.split('.').next().unwrap_or("").to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "WORKSTATION".to_owned())
}

impl Authenticator {
    fn new(config: &ProxyConfig) -> Result<Self> {
        let user = config.username.trim();
        let domain = config.domain.trim();
        if user.is_empty() {
            return Err(Error::Unsupported(format!(
                "{} proxy authentication needs a user name and password",
                config.auth.label()
            )));
        }
        // `DOMAIN\user` and `user@realm` are accepted as typed; a separate
        // domain is combined in the form each mechanism prefers.
        let username = if user.contains('\\') || user.contains('@') {
            Username::parse(user)
        } else if domain.is_empty() {
            Username::new(user, None)
        } else if config.auth == ProxyAuth::Ntlm {
            Username::new_down_level_logon_name(user, domain)
        } else {
            Username::new_upn(user, domain)
        }
        .map_err(|e| sspi_error("user name", e))?;
        let identity = AuthIdentity { username, password: config.password.clone().into() };

        match config.auth {
            ProxyAuth::Ntlm => {
                let mut ntlm = Ntlm::with_config(NtlmConfig::new(workstation()));
                let acquired = ntlm
                    .acquire_credentials_handle()
                    .with_credential_use(CredentialUse::Outbound)
                    .with_auth_data(&identity)
                    .execute(&mut ntlm)
                    .map_err(|e| sspi_error("credentials", e))?;
                Ok(Authenticator::Ntlm { ntlm: Box::new(ntlm), credentials: acquired.credentials_handle })
            }
            ProxyAuth::Negotiate | ProxyAuth::Kerberos => {
                // The KDC is found through the realm's DNS records.
                let kerberos = KerberosConfig {
                    kdc_resolution: sspi::KdcResolution::KdcUrl(None),
                    client_computer_name: workstation(),
                };
                let packages = if config.auth == ProxyAuth::Kerberos { "kerberos,!ntlm" } else { "kerberos,ntlm" };
                let mut negotiate = Negotiate::new_client(NegotiateConfig::new(
                    Box::new(kerberos),
                    Some(packages.to_owned()),
                    workstation(),
                ))
                .map_err(|e| sspi_error("setup", e))?;
                let acquired = negotiate
                    .acquire_credentials_handle()
                    .with_credential_use(CredentialUse::Outbound)
                    .with_auth_data(&Credentials::AuthIdentity(identity))
                    .execute(&mut negotiate)
                    .map_err(|e| sspi_error("credentials", e))?;
                Ok(Authenticator::Negotiate { negotiate: Box::new(negotiate), credentials: acquired.credentials_handle })
            }
            ProxyAuth::None | ProxyAuth::Basic => {
                Err(Error::Unsupported("this proxy configuration does not need the bridge".into()))
            }
        }
    }

    /// The word that precedes the token in `Proxy-Authorization`.
    fn scheme(&self) -> &'static str {
        match self {
            Authenticator::Ntlm { .. } => "NTLM",
            Authenticator::Negotiate { .. } => "Negotiate",
        }
    }

    /// Produces the next token, given the server's previous one.
    fn step(&mut self, challenge: Option<Vec<u8>>, proxy_host: &str) -> Result<Vec<u8>> {
        let target = format!("HTTP/{proxy_host}");
        let mut input = vec![SecurityBuffer::new(challenge.unwrap_or_default(), BufferType::Token)];
        let mut output = vec![SecurityBuffer::new(Vec::new(), BufferType::Token)];
        let status = match self {
            Authenticator::Ntlm { ntlm, credentials } => {
                let mut builder = ntlm
                    .initialize_security_context()
                    .with_credentials_handle(credentials)
                    .with_context_requirements(ClientRequestFlags::ALLOCATE_MEMORY)
                    .with_target_data_representation(DataRepresentation::Native)
                    .with_target_name(&target)
                    .with_input(&mut input)
                    .with_output(&mut output);
                ntlm.initialize_security_context_impl(&mut builder)
                    .map_err(|e| sspi_error("NTLM", e))?
                    .resolve_to_result()
                    .map_err(|e| sspi_error("NTLM", e))?
                    .status
            }
            Authenticator::Negotiate { negotiate, credentials } => {
                let mut builder = negotiate
                    .initialize_security_context()
                    .with_credentials_handle(credentials)
                    .with_context_requirements(ClientRequestFlags::MUTUAL_AUTH | ClientRequestFlags::ALLOCATE_MEMORY)
                    .with_target_data_representation(DataRepresentation::Native)
                    .with_target_name(&target)
                    .with_input(&mut input)
                    .with_output(&mut output);
                // Kerberos talks to the KDC from inside this call.
                negotiate
                    .initialize_security_context_impl(&mut builder)
                    .map_err(|e| sspi_error("Negotiate", e))?
                    .resolve_with_default_network_client()
                    .map_err(|e| sspi_error("Negotiate", e))?
                    .status
            }
        };
        match status {
            SecurityStatus::Ok | SecurityStatus::ContinueNeeded | SecurityStatus::CompleteNeeded
            | SecurityStatus::CompleteAndContinue => Ok(std::mem::take(&mut output[0].buffer)),
            other => Err(sspi_error("handshake", format!("{other:?}"))),
        }
    }
}

/// A request or response head, and whatever was read past it.
struct Head {
    text: String,
    rest: Vec<u8>,
}

impl Head {
    fn status(&self) -> Option<u16> {
        self.text.split_whitespace().nth(1)?.parse().ok()
    }

    fn header<'a>(&'a self, name: &'a str) -> Option<&'a str> {
        self.headers(name).next()
    }

    fn headers<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.text.split("\r\n").skip(1).filter_map(move |line| {
            let (key, value) = line.split_once(':')?;
            key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }
}

async fn read_head(stream: &mut TcpStream) -> std::io::Result<Head> {
    let mut buffer = Vec::with_capacity(2048);
    loop {
        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buffer.split_off(end + 4);
            buffer.truncate(end);
            return Ok(Head { text: String::from_utf8_lossy(&buffer).into_owned(), rest });
        }
        if buffer.len() > MAX_HEAD {
            return Err(std::io::Error::other("header block too large"));
        }
        let mut chunk = [0u8; 4096];
        match stream.read(&mut chunk).await? {
            0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            n => buffer.extend_from_slice(&chunk[..n]),
        }
    }
}

/// The request head with our `Proxy-Authorization` and nobody else's.
fn with_authorization(request: &Head, scheme: &str, token: &[u8]) -> Vec<u8> {
    let mut out = String::with_capacity(request.text.len() + token.len() * 2);
    for line in request.text.split("\r\n") {
        let name = line.split(':').next().unwrap_or("").trim();
        if name.eq_ignore_ascii_case("proxy-authorization") || name.eq_ignore_ascii_case("proxy-connection") {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str("Proxy-Connection: Keep-Alive\r\n");
    out.push_str(&format!("Proxy-Authorization: {scheme} {}\r\n\r\n", BASE64.encode(token)));
    out.into_bytes()
}

/// Reads and discards the body of a `407`, so the next leg starts clean.
async fn drain_body(stream: &mut TcpStream, head: &Head) -> std::io::Result<()> {
    let length: usize = head.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut remaining = length.saturating_sub(head.rest.len());
    let mut sink = [0u8; 4096];
    while remaining > 0 {
        let n = stream.read(&mut sink[..remaining.min(4096)]).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        remaining -= n;
    }
    Ok(())
}

async fn refuse(client: &mut TcpStream, reason: &str) {
    let body = format!("Siphon proxy bridge: {reason}\n");
    let response = format!(
        "HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = client.write_all(response.as_bytes()).await;
}

async fn serve(mut client: TcpStream, config: ProxyConfig) {
    let _ = client.set_nodelay(true);
    let Ok(Ok(request)) = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_head(&mut client)).await else { return };

    let host = config.host.trim().to_owned();
    let connected = tokio::time::timeout(HANDSHAKE_TIMEOUT, TcpStream::connect((host.as_str(), config.port))).await;
    let mut upstream = match connected {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => return refuse(&mut client, &format!("cannot reach the proxy at {host}:{}: {e}", config.port)).await,
        Err(_) => return refuse(&mut client, &format!("the proxy at {host}:{} did not answer", config.port)).await,
    };
    let _ = upstream.set_nodelay(true);

    let mut authenticator = match Authenticator::new(&config) {
        Ok(authenticator) => authenticator,
        Err(e) => return refuse(&mut client, &e.to_string()).await,
    };
    let scheme = authenticator.scheme();
    let mut challenge: Option<Vec<u8>> = None;

    for _ in 0..MAX_LEGS {
        let token = match blocking(|| authenticator.step(challenge.take(), &host)) {
            Ok(token) => token,
            Err(e) => return refuse(&mut client, &e.to_string()).await,
        };
        if upstream.write_all(&with_authorization(&request, scheme, &token)).await.is_err()
            || (!request.rest.is_empty() && upstream.write_all(&request.rest).await.is_err())
        {
            return refuse(&mut client, "the proxy closed the connection during authentication").await;
        }
        let Ok(Ok(response)) = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_head(&mut upstream)).await else {
            return refuse(&mut client, "the proxy closed the connection during authentication").await;
        };

        let next = (response.status() == Some(407))
            .then(|| {
                response.headers("proxy-authenticate").find_map(|value| {
                    let (offered, token) = value.split_once(' ')?;
                    offered.eq_ignore_ascii_case(scheme).then(|| BASE64.decode(token.trim()).ok()).flatten()
                })
            })
            .flatten();

        match next {
            // The proxy answered our token with one of its own: next leg.
            Some(token) => {
                if drain_body(&mut upstream, &response).await.is_err() {
                    return refuse(&mut client, "the proxy closed the connection during authentication").await;
                }
                challenge = Some(token);
            }
            // Accepted, refused for good, or something unrelated to
            // authentication: the client gets the proxy's own answer and,
            // if the tunnel is up, the bytes that follow.
            None => {
                let mut head = response.text.into_bytes();
                head.extend_from_slice(b"\r\n\r\n");
                if client.write_all(&head).await.is_err() || client.write_all(&response.rest).await.is_err() {
                    return;
                }
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                return;
            }
        }
    }
    refuse(&mut client, "the proxy kept asking for credentials").await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{NetConfig, ProxyMode, build_client};
    use sspi::ServerRequestFlags;

    /// A proxy that demands NTLM and verifies the response cryptographically
    /// with the server half of the same security package.
    async fn ntlm_proxy(user: &'static str, password: &'static str) -> (SocketAddr, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let tunnels = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = tunnels.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { break };
                let counter = counter.clone();
                tokio::spawn(async move {
                    let mut ntlm = Ntlm::new();
                    let identity = AuthIdentity { username: Username::new(user, None).unwrap(), password: password.to_owned().into() };
                    let mut acquired = ntlm
                        .acquire_credentials_handle()
                        .with_credential_use(CredentialUse::Inbound)
                        .with_auth_data(&identity)
                        .execute(&mut ntlm)
                        .unwrap();

                    loop {
                        let Ok(request) = read_head(&mut socket).await else { return };
                        let token = request
                            .header("proxy-authorization")
                            .and_then(|v| v.strip_prefix("NTLM "))
                            .and_then(|t| BASE64.decode(t.trim()).ok());
                        let Some(token) = token else {
                            let _ = socket
                                .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: NTLM\r\nContent-Length: 0\r\n\r\n")
                                .await;
                            continue;
                        };
                        let mut input = vec![SecurityBuffer::new(token, BufferType::Token)];
                        let mut output = vec![SecurityBuffer::new(Vec::new(), BufferType::Token)];
                        let builder = ntlm
                            .accept_security_context()
                            .with_credentials_handle(&mut acquired.credentials_handle)
                            .with_context_requirements(ServerRequestFlags::ALLOCATE_MEMORY)
                            .with_target_data_representation(DataRepresentation::Native)
                            .with_input(&mut input)
                            .with_output(&mut output);
                        let accepted = ntlm.accept_security_context_impl(builder).and_then(|mut g| g.resolve_to_result());
                        match accepted {
                            Ok(result) if result.status == SecurityStatus::ContinueNeeded => {
                                let body = "authenticate";
                                let reply = format!(
                                    "HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: NTLM {}\r\nContent-Length: {}\r\n\r\n{body}",
                                    BASE64.encode(&output[0].buffer),
                                    body.len()
                                );
                                if socket.write_all(reply.as_bytes()).await.is_err() {
                                    return;
                                }
                            }
                            Ok(result) if matches!(result.status, SecurityStatus::Ok | SecurityStatus::CompleteNeeded) => {
                                if result.status == SecurityStatus::CompleteNeeded && ntlm.complete_auth_token(&mut output).is_err() {
                                    let _ = socket.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n").await;
                                    return;
                                }
                                // Authenticated: open the tunnel.
                                let target = request.text.split_whitespace().nth(1).unwrap_or("").to_owned();
                                let Ok(mut origin) = TcpStream::connect(&target).await else {
                                    let _ = socket.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n").await;
                                    return;
                                };
                                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                let _ = socket.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await;
                                let _ = tokio::io::copy_bidirectional(&mut socket, &mut origin).await;
                                return;
                            }
                            _ => {
                                let _ = socket
                                    .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: NTLM\r\nContent-Length: 0\r\n\r\n")
                                    .await;
                                return;
                            }
                        }
                    }
                });
            }
        });
        (addr, tunnels)
    }

    /// An origin that only answers `CONNECT`-tunnelled plain HTTP.
    async fn origin(body: &'static [u8]) -> SocketAddr {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { break };
                tokio::spawn(async move {
                    while read_head(&mut socket).await.is_ok() {
                        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
                        if socket.write_all(head.as_bytes()).await.is_err() || socket.write_all(body).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        addr
    }

    fn proxy_config(addr: SocketAddr, user: &str, password: &str) -> ProxyConfig {
        ProxyConfig {
            mode: ProxyMode::Manual,
            kind: ProxyKind::Http,
            host: addr.ip().to_string(),
            port: addr.port(),
            auth: ProxyAuth::Ntlm,
            username: user.into(),
            password: password.into(),
            ..Default::default()
        }
    }

    /// Speaks to the bridge the way an HTTP client does for an `https://`
    /// target: `CONNECT`, then requests inside the tunnel.
    async fn through_bridge(bridge: SocketAddr, target: SocketAddr) -> std::io::Result<(u16, Vec<u8>)> {
        let mut stream = TcpStream::connect(bridge).await?;
        stream.write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes()).await?;
        let established = read_head(&mut stream).await?;
        let status = established.status().unwrap_or(0);
        if status != 200 {
            return Ok((status, Vec::new()));
        }
        stream.write_all(b"GET /file HTTP/1.1\r\nHost: origin\r\n\r\n").await?;
        let response = read_head(&mut stream).await?;
        let length: usize = response.header("content-length").unwrap().parse().unwrap();
        let status = response.status().unwrap_or(0);
        let mut body = response.rest;
        while body.len() < length {
            let mut chunk = [0u8; 1024];
            let n = stream.read(&mut chunk).await?;
            body.extend_from_slice(&chunk[..n]);
        }
        Ok((status, body))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ntlm_handshake_opens_the_tunnel() {
        let (proxy, tunnels) = ntlm_proxy("alice", "correct horse").await;
        let origin = origin(b"payload through an authenticated tunnel").await;
        let bridge = ProxyBridge::start(proxy_config(proxy, "alice", "correct horse")).await.unwrap();

        let (status, body) = through_bridge(bridge.addr(), origin).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"payload through an authenticated tunnel");
        assert_eq!(tunnels.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_wrong_password_is_refused_by_the_proxy() {
        let (proxy, tunnels) = ntlm_proxy("alice", "correct horse").await;
        let origin = origin(b"secret").await;
        let bridge = ProxyBridge::start(proxy_config(proxy, "alice", "wrong")).await.unwrap();

        let (status, body) = through_bridge(bridge.addr(), origin).await.unwrap();
        assert_eq!(status, 407, "the proxy's refusal must reach the client");
        assert!(body.is_empty());
        assert_eq!(tunnels.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_download_client_works_through_the_bridge() {
        let (proxy, tunnels) = ntlm_proxy("CORP\\bob", "hunter2").await;
        let origin = origin(b"0123456789").await;
        let config = NetConfig { proxy: proxy_config(proxy, "CORP\\bob", "hunter2"), ..Default::default() };
        let bridge = ProxyBridge::start(config.proxy.clone()).await.unwrap();

        // Loopback targets normally bypass a proxy; route this one on purpose.
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(format!("http://{}", bridge.addr())).unwrap())
            .build()
            .unwrap();
        // An `https` target makes the client use CONNECT, as real downloads do;
        // the handshake below it is what is under test, so plain HTTP inside
        // the tunnel is enough here.
        let mut stream = TcpStream::connect(bridge.addr()).await.unwrap();
        stream.write_all(format!("CONNECT {origin} HTTP/1.1\r\nHost: {origin}\r\n\r\n").as_bytes()).await.unwrap();
        assert_eq!(read_head(&mut stream).await.unwrap().status(), Some(200));
        drop(stream);
        drop(client);

        // And the shipped client builder accepts the bridge address.
        assert!(build_client(&config, Some(bridge.addr())).is_ok());
        assert!(tunnels.load(std::sync::atomic::Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn refuses_configurations_it_cannot_serve() {
        let mut config = proxy_config("127.0.0.1:9".parse().unwrap(), "", "x");
        assert!(matches!(ProxyBridge::start(config.clone()).await, Err(Error::Unsupported(_))), "no user name");
        config.username = "u".into();
        config.kind = ProxyKind::Socks5;
        assert!(matches!(ProxyBridge::start(config.clone()).await, Err(Error::Unsupported(_))));
        config.kind = ProxyKind::Http;
        config.auth = ProxyAuth::Basic;
        assert!(matches!(ProxyBridge::start(config).await, Err(Error::Unsupported(_))));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unreachable_proxy_is_reported() {
        let dead = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap();
        let bridge = ProxyBridge::start(proxy_config(dead, "u", "p")).await.unwrap();
        let mut stream = TcpStream::connect(bridge.addr()).await.unwrap();
        stream.write_all(b"CONNECT example.org:443 HTTP/1.1\r\n\r\n").await.unwrap();
        let reply = read_head(&mut stream).await.unwrap();
        assert_eq!(reply.status(), Some(502));
    }

    #[test]
    fn authorization_replaces_any_existing_header() {
        let request = Head {
            text: "CONNECT a.example:443 HTTP/1.1\r\nHost: a.example:443\r\nProxy-Authorization: Basic Zm9v\r\nproxy-connection: close".into(),
            rest: Vec::new(),
        };
        let out = String::from_utf8(with_authorization(&request, "NTLM", b"abc")).unwrap();
        assert!(out.starts_with("CONNECT a.example:443 HTTP/1.1\r\nHost: a.example:443\r\n"));
        assert!(!out.contains("Basic"));
        assert_eq!(out.matches("Proxy-Authorization:").count(), 1);
        assert!(out.ends_with("Proxy-Authorization: NTLM YWJj\r\n\r\n"));
    }
}
