//! Error taxonomy.
//!
//! The engine never asks "what went wrong" in the abstract; it asks one of
//! three questions: can this worker retry, has the link expired, or is the
//! download dead. [`Error::class`] answers that in one place.

use std::time::Duration;

/// How the engine should react to an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorClass {
    /// Worth retrying on the same URL after a pause.
    Transient,
    /// The URL stopped working but the file identity is still good. Keep the
    /// partial data and wait for a fresh link.
    Expired,
    /// Retrying the same thing cannot succeed.
    Fatal,
    /// The user (or a shutdown) asked the work to stop.
    Cancelled,
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum Error {
    #[error("the link has expired or access was denied (HTTP {0})")]
    Expired(u16),

    #[error("the server is busy (HTTP {code})")]
    Busy { code: u16, retry_after: Option<Duration> },

    #[error("the server refused the request (HTTP {0})")]
    Status(u16),

    #[error("network error: {0}")]
    Network(String),

    #[error("the connection stalled")]
    Timeout,

    #[error("the file changed on the server since this download started")]
    Changed,

    #[error("the fresh link points to a different file ({0})")]
    Mismatch(String),

    #[error("the server ignored the requested byte range")]
    RangeIgnored,

    #[error("the server closed the connection before sending the whole file")]
    Truncated,

    #[error("not enough free space: {needed} bytes needed, {available} available")]
    DiskFull { needed: u64, available: u64 },

    #[error("disk error: {0}")]
    Disk(String),

    #[error("invalid address: {0}")]
    InvalidUrl(String),

    #[error("{0}")]
    Unsupported(String),

    #[error("{0}")]
    Protocol(String),

    #[error("cancelled")]
    Cancelled,
}

impl Error {
    pub fn class(&self) -> ErrorClass {
        match self {
            Error::Expired(_) => ErrorClass::Expired,
            Error::Busy { .. } | Error::Network(_) | Error::Timeout | Error::Truncated => {
                ErrorClass::Transient
            }
            Error::Cancelled => ErrorClass::Cancelled,
            Error::Status(_)
            | Error::Changed
            | Error::Mismatch(_)
            | Error::RangeIgnored
            | Error::DiskFull { .. }
            | Error::Disk(_)
            | Error::InvalidUrl(_)
            | Error::Unsupported(_)
            | Error::Protocol(_) => ErrorClass::Fatal,
        }
    }

    pub fn is_transient(&self) -> bool {
        self.class() == ErrorClass::Transient
    }

    /// Maps a response status that is not a success for a payload request.
    pub fn from_status(code: u16, retry_after: Option<Duration>) -> Error {
        match code {
            // The four ways a signed, tokenised or deleted link says goodbye.
            401 | 403 | 404 | 410 => Error::Expired(code),
            408 | 425 | 429 | 500 | 502 | 503 | 504 | 509 | 520..=527 => {
                Error::Busy { code, retry_after }
            }
            _ => Error::Status(code),
        }
    }

    /// Whether the user can usefully press "retry" on a failed download.
    pub fn retryable(&self) -> bool {
        !matches!(
            self,
            Error::InvalidUrl(_) | Error::Unsupported(_) | Error::Mismatch(_) | Error::Cancelled
        )
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        use std::io::ErrorKind as K;
        match e.kind() {
            K::StorageFull | K::QuotaExceeded => Error::DiskFull { needed: 0, available: 0 },
            _ => Error::Disk(e.to_string()),
        }
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            return Error::Timeout;
        }
        if e.is_builder() {
            return Error::InvalidUrl(root_cause(&e));
        }
        if e.is_redirect() {
            return Error::Protocol(format!("redirect loop: {}", root_cause(&e)));
        }
        Error::Network(root_cause(&e))
    }
}

/// reqwest wraps the interesting message several layers deep ("error sending
/// request" -> "client error (Connect)" -> "tcp connect error" -> "Connection
/// refused"). The innermost one is what a person wants to read.
fn root_cause(e: &dyn std::error::Error) -> String {
    let mut cur: &dyn std::error::Error = e;
    while let Some(next) = cur.source() {
        cur = next;
    }
    let msg = cur.to_string();
    // Drop the trailing "(os error 61)" noise; the text before it already says it.
    match msg.rfind(" (os error ") {
        Some(i) => msg[..i].to_owned(),
        None => msg,
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_classification() {
        assert_eq!(Error::from_status(403, None).class(), ErrorClass::Expired);
        assert_eq!(Error::from_status(410, None).class(), ErrorClass::Expired);
        assert_eq!(Error::from_status(503, None).class(), ErrorClass::Transient);
        assert_eq!(Error::from_status(429, None).class(), ErrorClass::Transient);
        assert_eq!(Error::from_status(400, None).class(), ErrorClass::Fatal);
        assert_eq!(Error::from_status(416, None).class(), ErrorClass::Fatal);
    }
}
