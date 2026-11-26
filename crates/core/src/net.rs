//! The shared HTTP client and how it reaches the network.

use std::net::SocketAddr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Sent when the browser did not hand over its own user agent. Many file
/// hosts refuse clients that do not look like a browser.
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/158.0.0.0 Safari/537.36";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyMode {
    /// Whatever the operating system is configured to use.
    #[default]
    System,
    /// Connect straight to the server.
    Direct,
    Manual,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyKind {
    #[default]
    Http,
    Https,
    Socks5,
}

impl ProxyKind {
    pub const ALL: [ProxyKind; 3] = [ProxyKind::Http, ProxyKind::Https, ProxyKind::Socks5];

    pub fn scheme(self) -> &'static str {
        match self {
            ProxyKind::Http => "http",
            ProxyKind::Https => "https",
            // `socks5h` resolves names at the proxy, which is what a proxy user expects.
            ProxyKind::Socks5 => "socks5h",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ProxyKind::Http => "HTTP",
            ProxyKind::Https => "HTTPS",
            ProxyKind::Socks5 => "SOCKS5",
        }
    }
}

/// How the client proves itself to the proxy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyAuth {
    #[default]
    None,
    Basic,
    Ntlm,
    /// SPNEGO: Kerberos when a ticket can be obtained, NTLM otherwise.
    Negotiate,
    Kerberos,
}

impl ProxyAuth {
    pub const ALL: [ProxyAuth; 5] =
        [ProxyAuth::None, ProxyAuth::Basic, ProxyAuth::Ntlm, ProxyAuth::Negotiate, ProxyAuth::Kerberos];

    pub fn label(self) -> &'static str {
        match self {
            ProxyAuth::None => "None",
            ProxyAuth::Basic => "Basic",
            ProxyAuth::Ntlm => "NTLM",
            ProxyAuth::Negotiate => "Negotiate",
            ProxyAuth::Kerberos => "Kerberos",
        }
    }

    /// Challenge-response schemes authenticate a *connection* through a
    /// multi-leg handshake, which the HTTP client cannot do on its own. They
    /// are handled by the in-process bridge in [`crate::proxy_bridge`].
    pub fn needs_bridge(self) -> bool {
        matches!(self, ProxyAuth::Ntlm | ProxyAuth::Negotiate | ProxyAuth::Kerberos)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxyConfig {
    pub mode: ProxyMode,
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    pub auth: ProxyAuth,
    pub username: String,
    pub password: String,
    /// Windows domain or Kerberos realm, when the account needs one.
    pub domain: String,
    /// Comma-separated hosts that bypass the proxy.
    pub bypass: String,
}

impl ProxyConfig {
    pub fn is_manual(&self) -> bool {
        self.mode == ProxyMode::Manual && !self.host.trim().is_empty() && self.port != 0
    }

    pub fn url(&self) -> String {
        format!("{}://{}:{}", self.kind.scheme(), self.host.trim(), self.port)
    }

    pub fn needs_bridge(&self) -> bool {
        self.is_manual() && self.auth.needs_bridge()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetConfig {
    pub proxy: ProxyConfig,
    /// Empty means [`DEFAULT_USER_AGENT`].
    pub user_agent: String,
    pub connect_timeout_secs: u64,
    /// A connection that delivers nothing for this long is dropped and retried.
    pub stall_timeout_secs: u64,
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            proxy: ProxyConfig::default(),
            user_agent: String::new(),
            connect_timeout_secs: 20,
            stall_timeout_secs: 30,
        }
    }
}

impl NetConfig {
    pub fn user_agent(&self) -> &str {
        let ua = self.user_agent.trim();
        if ua.is_empty() { DEFAULT_USER_AGENT } else { ua }
    }
}

/// Builds the one client every download shares.
///
/// Sharing it is the point: the connection pool lives inside, so a worker
/// that finishes a range hands its warm socket (TCP and TLS already set up)
/// to whichever worker asks next instead of paying for a new handshake.
///
/// `bridge` is the local address of the authentication bridge when the proxy
/// needs NTLM or Negotiate.
pub fn build_client(config: &NetConfig, bridge: Option<SocketAddr>) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .user_agent(config.user_agent())
        .connect_timeout(Duration::from_secs(config.connect_timeout_secs.clamp(3, 300)))
        .read_timeout(Duration::from_secs(config.stall_timeout_secs.clamp(5, 600)))
        .pool_max_idle_per_host(64)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_nodelay(true)
        .tcp_keepalive(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(12))
        .http1_only();

    let proxy = &config.proxy;
    match proxy.mode {
        ProxyMode::System => {}
        ProxyMode::Direct => builder = builder.no_proxy(),
        ProxyMode::Manual if !proxy.is_manual() => builder = builder.no_proxy(),
        ProxyMode::Manual => {
            let target = match (proxy.auth.needs_bridge(), bridge) {
                (true, Some(addr)) => format!("http://{addr}"),
                (true, None) => {
                    return Err(Error::Unsupported(format!(
                        "{} proxy authentication needs the local bridge, which is not running",
                        proxy.auth.label()
                    )));
                }
                (false, _) => proxy.url(),
            };
            let mut p = reqwest::Proxy::all(&target)
                .map_err(|e| Error::InvalidUrl(format!("proxy address: {e}")))?;
            if proxy.auth == ProxyAuth::Basic && !proxy.username.is_empty() {
                p = p.basic_auth(&proxy.username, &proxy.password);
            }
            p = p.no_proxy(reqwest::NoProxy::from_string(&bypass_list(&proxy.bypass)));
            builder = builder.proxy(p);
        }
    }

    builder.build().map_err(|e| Error::Protocol(format!("could not set up networking: {e}")))
}

/// Loopback never goes through a proxy, whatever the user typed.
fn bypass_list(user: &str) -> String {
    let mut parts: Vec<&str> = user.split([',', ';', ' ']).map(str::trim).filter(|s| !s.is_empty()).collect();
    for local in ["localhost", "127.0.0.1", "::1"] {
        if !parts.contains(&local) {
            parts.push(local);
        }
    }
    parts.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_for_every_mode() {
        let mut cfg = NetConfig::default();
        assert!(build_client(&cfg, None).is_ok());

        cfg.proxy.mode = ProxyMode::Direct;
        assert!(build_client(&cfg, None).is_ok());

        cfg.proxy = ProxyConfig {
            mode: ProxyMode::Manual,
            kind: ProxyKind::Socks5,
            host: "proxy.internal".into(),
            port: 1080,
            auth: ProxyAuth::Basic,
            username: "u".into(),
            password: "p".into(),
            ..Default::default()
        };
        assert!(build_client(&cfg, None).is_ok());

        cfg.proxy.kind = ProxyKind::Http;
        cfg.proxy.auth = ProxyAuth::Ntlm;
        assert!(build_client(&cfg, None).is_err(), "NTLM without a bridge must not silently go unauthenticated");
        assert!(build_client(&cfg, Some("127.0.0.1:9".parse().unwrap())).is_ok());
    }

    #[test]
    fn manual_without_a_host_means_direct() {
        let mut cfg = NetConfig::default();
        cfg.proxy.mode = ProxyMode::Manual;
        assert!(!cfg.proxy.is_manual());
        assert!(build_client(&cfg, None).is_ok());
    }

    #[test]
    fn loopback_always_bypasses() {
        assert_eq!(bypass_list("intranet.corp; *.local"), "intranet.corp,*.local,localhost,127.0.0.1,::1");
        assert_eq!(bypass_list(""), "localhost,127.0.0.1,::1");
    }
}
