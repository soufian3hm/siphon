//! What every byte source has in common.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::ftp::{FtpBody, FtpSource};
use crate::http::{HttpBody, HttpSource};

/// Fingerprints a server attaches to one version of a file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Validators {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

impl Validators {
    /// True when both sides carry a comparable fingerprint and they differ.
    /// A missing fingerprint proves nothing and is never a conflict.
    pub fn conflicts_with(&self, other: &Validators) -> bool {
        fn opaque(tag: &str) -> &str {
            tag.strip_prefix("W/").unwrap_or(tag)
        }
        match (&self.etag, &other.etag) {
            (Some(a), Some(b)) => return opaque(a) != opaque(b),
            // A server that stops sending its ETag has not changed the file.
            (Some(_), None) | (None, Some(_)) => return false,
            (None, None) => {}
        }
        match (&self.last_modified, &other.last_modified) {
            (Some(a), Some(b)) => a != b,
            _ => false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.etag.is_none() && self.last_modified.is_none()
    }
}

/// What the server said about the file before any payload was requested.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    /// `None` when the server does not announce a size.
    pub total: Option<u64>,
    /// The server proved it honours byte ranges, so the download can be
    /// segmented, paused and resumed.
    pub resumable: bool,
    pub validators: Validators,
    pub content_type: Option<String>,
    /// Best available name, already safe for the filesystem.
    pub file_name: String,
    /// The URL after redirects.
    pub final_url: String,
}

/// A place bytes come from.
#[derive(Debug)]
pub enum Source {
    Http(HttpSource),
    Ftp(FtpSource),
}

/// An open stream of payload bytes.
#[derive(Debug)]
pub enum Body {
    Http(HttpBody),
    Ftp(FtpBody),
}

impl Source {
    pub async fn probe(&self) -> Result<(Probe, Option<Body>)> {
        match self {
            Source::Http(s) => {
                let (probe, body) = s.probe().await?;
                Ok((probe, body.map(Body::Http)))
            }
            Source::Ftp(s) => {
                let probe = s.probe().await?;
                // Without a known size there are no ranges; hand over the
                // whole file as one stream, like an unranged HTTP reply.
                let body = if probe.resumable { None } else { Some(Body::Ftp(s.open(0, None).await?)) };
                Ok((probe, body))
            }
        }
    }

    /// Opens `[start, end)` of a file known to be `total` bytes long.
    pub async fn open(&self, start: u64, end: u64, total: u64) -> Result<Body> {
        match self {
            Source::Http(s) => s.open(start, end, total).await.map(Body::Http),
            Source::Ftp(s) => s.open(start, Some(end)).await.map(Body::Ftp),
        }
    }

    pub fn validators(&self) -> Validators {
        match self {
            Source::Http(s) => s.validators(),
            // FTP offers no per-response fingerprint to track.
            Source::Ftp(_) => Validators::default(),
        }
    }

    /// The concrete resource the validators describe.
    pub fn resource_url(&self) -> String {
        match self {
            Source::Http(s) => s.target_url(),
            Source::Ftp(s) => s.display_url(),
        }
    }

    /// Tells the source which version of the file a resumed download expects.
    pub fn expect(&self, validators: Validators) {
        match self {
            Source::Http(s) => s.expect(validators),
            Source::Ftp(_) => {}
        }
    }
}

impl Body {
    /// The next piece of payload, or `None` at the end of the stream.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>> {
        match self {
            Body::Http(b) => b.chunk().await,
            Body::Ftp(b) => b.chunk().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(etag: Option<&str>, lm: Option<&str>) -> Validators {
        Validators { etag: etag.map(Into::into), last_modified: lm.map(Into::into) }
    }

    #[test]
    fn validator_conflicts() {
        assert!(v(Some("\"a\""), None).conflicts_with(&v(Some("\"b\""), None)));
        assert!(!v(Some("\"a\""), None).conflicts_with(&v(Some("W/\"a\""), None)));
        assert!(!v(Some("\"a\""), None).conflicts_with(&v(None, None)));
        assert!(!v(None, None).conflicts_with(&v(Some("\"a\""), Some("x"))));
        assert!(v(None, Some("Mon")).conflicts_with(&v(None, Some("Tue"))));
        // An ETag on either side outranks dates: equal tags, different dates is fine.
        assert!(!v(Some("\"a\""), Some("Mon")).conflicts_with(&v(Some("\"a\""), Some("Tue"))));
    }
}
