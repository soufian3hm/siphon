//! FTP source.
//!
//! FTP has no byte ranges, but it has `REST`: "start the next transfer at
//! this offset". A range is therefore a transfer that begins at its start and
//! is cut off by the client at its end. Each worker logs in on its own
//! control connection, which is exactly what lets FTP files be segmented the
//! same way HTTP files are.

use std::net::SocketAddr;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use percent_encoding::percent_decode_str;
use suppaftp::tokio::{AsyncFtpStream, AsyncNoTlsStream};
use suppaftp::types::FileType;
use suppaftp::{FtpError, Status as FtpStatus};
use tokio::io::AsyncReadExt;

use crate::error::{Error, Result};
use crate::filename;
use crate::source::{Probe, Validators};
use crate::types::Request;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const STALL_TIMEOUT: Duration = Duration::from_secs(30);
const READ_CHUNK: usize = 128 * 1024;

type Transfer = suppaftp::tokio::TransferStream<AsyncNoTlsStream>;

#[derive(Debug)]
pub struct FtpSource {
    host: String,
    port: u16,
    user: String,
    password: String,
    /// Server-side path, percent-decoded.
    path: String,
    url: String,
}

/// An open `RETR`, limited to the bytes its range covers.
pub struct FtpBody {
    /// Kept alive for as long as the data connection is read; dropping it
    /// closes the session, which is how a range is cut short.
    _control: AsyncFtpStream,
    transfer: Transfer,
    remaining: u64,
}

impl std::fmt::Debug for FtpBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FtpBody").field("remaining", &self.remaining).finish()
    }
}

impl FtpSource {
    pub fn new(request: &Request) -> Result<Self> {
        let parsed = url::Url::parse(request.url.trim())
            .map_err(|e| Error::InvalidUrl(format!("{}: {e}", request.url.trim())))?;
        match parsed.scheme() {
            "ftp" => {}
            "ftps" => return Err(Error::Unsupported("FTP over TLS (ftps://) is not supported yet.".into())),
            other => return Err(Error::InvalidUrl(format!("unsupported scheme \"{other}\""))),
        }
        let host = parsed
            .host_str()
            .filter(|h| !h.is_empty())
            .ok_or_else(|| Error::InvalidUrl("the address has no host".into()))?
            .trim_matches(['[', ']'])
            .to_owned();
        let decode = |s: &str| percent_decode_str(s).decode_utf8_lossy().into_owned();
        // `;type=i` is a URL-level transfer hint, not part of the file name.
        let raw_path = parsed.path().split(";type=").next().unwrap_or("");
        let path = decode(raw_path);
        if path.is_empty() || path.ends_with('/') {
            return Err(Error::InvalidUrl("the address points to a folder, not a file".into()));
        }
        let (user, password) = match parsed.username() {
            "" => ("anonymous".to_owned(), "anonymous@".to_owned()),
            user => (decode(user), parsed.password().map(decode).unwrap_or_default()),
        };
        Ok(Self { host, port: parsed.port().unwrap_or(21), user, password, path, url: parsed.to_string() })
    }

    /// The address without credentials, safe to show and to log.
    pub fn display_url(&self) -> String {
        match url::Url::parse(&self.url) {
            Ok(mut u) => {
                let _ = u.set_username("");
                let _ = u.set_password(None);
                u.to_string()
            }
            Err(_) => self.url.clone(),
        }
    }

    async fn connect(&self) -> Result<AsyncFtpStream> {
        let mut last = Error::Network(format!("could not resolve {}", self.host));
        let addrs: Vec<SocketAddr> = match tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio::net::lookup_host((self.host.as_str(), self.port)),
        )
        .await
        {
            Ok(Ok(addrs)) => addrs.collect(),
            Ok(Err(e)) => return Err(Error::Network(format!("could not resolve {}: {e}", self.host))),
            Err(_) => return Err(Error::Timeout),
        };
        for addr in addrs {
            match AsyncFtpStream::connect_timeout(addr, CONNECT_TIMEOUT).await {
                Ok(mut ftp) => {
                    // Servers behind NAT announce their private address for
                    // the data connection; use the one we already reached.
                    ftp.set_passive_nat_workaround(true);
                    command(ftp.login(self.user.as_str(), self.password.as_str())).await?;
                    command(ftp.transfer_type(FileType::Binary)).await?;
                    return Ok(ftp);
                }
                Err(e) => last = map_error(e),
            }
        }
        Err(last)
    }

    pub async fn probe(&self) -> Result<Probe> {
        let mut ftp = self.connect().await?;
        // Without `SIZE` the file can still be fetched, front to back.
        let total = match command(ftp.size(&self.path)).await {
            Ok(size) => Some(size as u64),
            Err(Error::Expired(code)) => {
                // 550 to SIZE usually means "no such file". Some servers
                // refuse SIZE on principle; RETR will tell for sure.
                if command(ftp.mdtm(&self.path)).await.is_err() {
                    let _ = ftp.quit().await;
                    return Err(Error::Expired(code));
                }
                None
            }
            Err(e) if e.is_transient() => return Err(e),
            Err(_) => None,
        };
        let last_modified = command(ftp.mdtm(&self.path)).await.ok().map(|stamp| stamp.to_string());
        let resumable = total.is_some()
            && match command(ftp.feat()).await {
                Ok(features) => features.keys().any(|k| k.eq_ignore_ascii_case("REST")),
                // No FEAT is no evidence either way; nearly every server has REST.
                Err(_) => true,
            };
        let _ = ftp.quit().await;

        Ok(Probe {
            total,
            resumable,
            validators: Validators { etag: None, last_modified },
            content_type: None,
            file_name: filename::sanitize(&self.path),
            final_url: self.display_url(),
        })
    }

    /// Opens `[start, end)`. `end` of `None` reads to the end of the file.
    pub async fn open(&self, start: u64, end: Option<u64>) -> Result<FtpBody> {
        let mut ftp = self.connect().await?;
        if start > 0 {
            command(ftp.resume_transfer(start as usize)).await?;
        }
        let transfer = command(ftp.retr_as_stream(&self.path)).await.map_err(|e| match e {
            // A refused REST comes back on RETR. That is "no ranges here".
            Error::Status(_) if start > 0 => Error::RangeIgnored,
            other => other,
        })?;
        let remaining = end.map(|end| end.saturating_sub(start)).unwrap_or(u64::MAX);
        Ok(FtpBody { _control: ftp, transfer, remaining })
    }

    pub fn url(&self) -> &str {
        &self.url
    }
}

impl FtpBody {
    pub async fn chunk(&mut self) -> Result<Option<Bytes>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let want = READ_CHUNK.min(self.remaining.min(usize::MAX as u64) as usize);
        let mut buf = BytesMut::with_capacity(want);
        let mut limited = (&mut self.transfer).take(want as u64);
        let read = tokio::time::timeout(STALL_TIMEOUT, limited.read_buf(&mut buf))
            .await
            .map_err(|_| Error::Timeout)?
            .map_err(|e| Error::Network(e.to_string()))?;
        if read == 0 {
            return Ok(None);
        }
        self.remaining -= read as u64;
        Ok(Some(buf.freeze()))
    }
}

/// Runs one FTP command with a deadline and maps its error.
async fn command<T>(future: impl Future<Output = suppaftp::FtpResult<T>>) -> Result<T> {
    match tokio::time::timeout(COMMAND_TIMEOUT, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(map_error(e)),
        Err(_) => Err(Error::Timeout),
    }
}

fn map_error(error: FtpError) -> Error {
    match error {
        FtpError::ConnectionError(e) => Error::Network(e.to_string()),
        FtpError::UnexpectedResponse(response) => {
            let code = response.status.code() as u16;
            match response.status {
                // "Too many connections from your address" lives here.
                FtpStatus::NotAvailable | FtpStatus::RequestFileActionIgnored => {
                    Error::Busy { code, retry_after: None }
                }
                FtpStatus::CannotOpenDataConnection | FtpStatus::TransferAborted => {
                    Error::Network(format!("FTP data connection failed ({code})"))
                }
                // Missing file, or no permission to read it.
                FtpStatus::FileUnavailable => Error::Expired(code),
                FtpStatus::NotLoggedIn => Error::Protocol(
                    "The FTP server rejected the user name or password. Put them in the address: ftp://user:password@host/file".into(),
                ),
                _ => Error::Status(code),
            }
        }
        FtpError::BadResponse => Error::Protocol("the FTP server sent a reply that could not be understood".into()),
        other => Error::Protocol(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parsing() {
        let s = FtpSource::new(&Request::new("ftp://ftp.example.org/pub/My%20File.iso")).unwrap();
        assert_eq!((s.host.as_str(), s.port), ("ftp.example.org", 21));
        assert_eq!((s.user.as_str(), s.password.as_str()), ("anonymous", "anonymous@"));
        assert_eq!(s.path, "/pub/My File.iso");

        let s = FtpSource::new(&Request::new("ftp://al%40corp:p%3Ass@10.0.0.5:2121/a/b.zip;type=i")).unwrap();
        assert_eq!((s.host.as_str(), s.port), ("10.0.0.5", 2121));
        assert_eq!((s.user.as_str(), s.password.as_str()), ("al@corp", "p:ss"));
        assert_eq!(s.path, "/a/b.zip");
        assert_eq!(s.display_url(), "ftp://10.0.0.5:2121/a/b.zip;type=i", "credentials never reach the UI");

        assert!(FtpSource::new(&Request::new("ftp://host/dir/")).is_err());
        assert!(FtpSource::new(&Request::new("ftp://host")).is_err());
        assert!(matches!(FtpSource::new(&Request::new("ftps://host/f")), Err(Error::Unsupported(_))));
        assert!(FtpSource::new(&Request::new("http://host/f")).is_err());
    }
}
