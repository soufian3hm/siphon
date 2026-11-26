//! HTTP and HTTPS source.

use std::time::Duration;

use bytes::Bytes;
use parking_lot::RwLock;
use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use reqwest::{RequestBuilder, Response, StatusCode};

use crate::error::{Error, Result};
use crate::filename;
use crate::source::{Probe, Validators};
use crate::types::Request;

/// Captured browser headers that must never be replayed: they describe the
/// browser's own connection or would fight the ones the engine sets.
const BLOCKED_HEADERS: [&str; 14] = [
    "host",
    "content-length",
    "content-type",
    "range",
    "if-range",
    "if-match",
    "if-none-match",
    "if-modified-since",
    "connection",
    "accept-encoding",
    "transfer-encoding",
    "upgrade",
    "te",
    "keep-alive",
];

/// Credentials are scoped to the origin they were captured for and are not
/// forwarded to a different host reached through a redirect.
const ORIGIN_BOUND_HEADERS: [&str; 3] = ["cookie", "authorization", "proxy-authorization"];

/// Where ranged requests are currently sent.
///
/// A link often redirects to a short-lived signed URL. Fetching every range
/// through the redirect would cost an extra round trip per range and, on
/// mirror networks, scatter the workers across servers with different
/// validators. So the redirect is resolved once and pinned; when the pinned
/// URL dies it is resolved again from the original link.
#[derive(Clone, Debug)]
struct Target {
    url: String,
    generation: u64,
    validators: Validators,
}

#[derive(Debug)]
pub struct HttpSource {
    client: reqwest::Client,
    request: Request,
    target: RwLock<Target>,
    /// Serialises re-resolution so a burst of failing workers triggers one.
    resolving: tokio::sync::Mutex<()>,
}

/// An open response body.
#[derive(Debug)]
pub struct HttpBody {
    response: Response,
}

impl HttpBody {
    pub async fn chunk(&mut self) -> Result<Option<Bytes>> {
        self.response.chunk().await.map_err(Error::from)
    }
}

impl HttpSource {
    pub fn new(client: reqwest::Client, request: Request) -> Result<Self> {
        let parsed = url::Url::parse(request.url.trim())
            .map_err(|e| Error::InvalidUrl(format!("{}: {e}", request.url.trim())))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(Error::InvalidUrl(format!("unsupported scheme \"{}\"", parsed.scheme())));
        }
        let target = Target { url: parsed.to_string(), generation: 0, validators: Validators::default() };
        let mut request = request;
        request.url = parsed.to_string();
        Ok(Self { client, request, target: RwLock::new(target), resolving: tokio::sync::Mutex::new(()) })
    }

    pub fn request(&self) -> &Request {
        &self.request
    }

    /// Asks the server to describe the file by requesting its first byte.
    ///
    /// A one-byte ranged `GET` beats `HEAD`: signed URLs are usually signed
    /// for `GET` only, many servers answer `HEAD` differently, and the reply
    /// proves range support instead of merely advertising it. When the server
    /// ignores the range and starts sending the whole file, that response is
    /// returned so the caller can keep reading instead of asking again.
    pub async fn probe(&self) -> Result<(Probe, Option<HttpBody>)> {
        let response = self
            .build(&self.request.url)
            .header(header::RANGE, "bytes=0-0")
            .send()
            .await?;
        let status = response.status();

        // `416` to `bytes=0-0` is how a server describes an empty file. Ask
        // again without a range and trust what comes back.
        let response = if status == StatusCode::RANGE_NOT_SATISFIABLE {
            self.build(&self.request.url).send().await?
        } else {
            response
        };
        let status = response.status();
        let headers = response.headers().clone();
        let final_url = response.url().to_string();

        let (total, resumable, body) = match status {
            StatusCode::PARTIAL_CONTENT => match content_range(&headers) {
                Some(ContentRange { first: 0, total: Some(total), .. }) => {
                    // One byte of body; reading it returns the socket to the pool.
                    let _ = response.bytes().await;
                    (Some(total), total > 0, None)
                }
                // A 206 that does not say what it covers cannot be trusted for
                // segmentation. Fall back to one plain stream.
                _ => {
                    drop(response);
                    let plain = self.build(&self.request.url).send().await?;
                    if !plain.status().is_success() {
                        return Err(status_error(&plain));
                    }
                    (plain.content_length(), false, Some(plain))
                }
            },
            s if s.is_success() => (response.content_length(), false, Some(response)),
            _ => return Err(status_error(&response)),
        };

        let validators = Validators {
            etag: header_str(&headers, header::ETAG),
            last_modified: header_str(&headers, header::LAST_MODIFIED),
        };
        let content_type = header_str(&headers, header::CONTENT_TYPE);
        let disposition = headers
            .get(header::CONTENT_DISPOSITION)
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
        let file_name = filename::resolve_from(
            disposition.as_deref(),
            &[final_url.as_str(), self.request.url.as_str()],
            content_type.as_deref(),
        );

        {
            let mut target = self.target.write();
            target.url = final_url.clone();
            target.generation += 1;
            target.validators = validators.clone();
        }

        let probe = Probe { total, resumable, validators, content_type, file_name, final_url };
        Ok((probe, body.map(|response| HttpBody { response })))
    }

    /// Opens the byte range `[start, end)`.
    ///
    /// `total` is the size the download was started with. Every response is
    /// checked against it and against the validators of the pinned URL, so a
    /// file that was replaced on the server is caught on the first byte
    /// rather than discovered as a corrupt archive.
    pub async fn open(&self, start: u64, end: u64, total: u64) -> Result<HttpBody> {
        let mut resolved_once = false;
        loop {
            let target = self.target.read().clone();
            match self.open_at(&target, start, end, total).await {
                Err(Error::Expired(code)) if !resolved_once && target.url != self.request.url => {
                    // The pinned redirect target died. The original link may
                    // still mint a fresh one.
                    resolved_once = true;
                    match self.resolve_again(target.generation, total).await {
                        Ok(()) => {}
                        // The network hiccuped while asking; let the worker retry.
                        Err(e) if e.is_transient() => return Err(e),
                        Err(Error::Expired(c)) => return Err(Error::Expired(c)),
                        // The original link now serves something else. What is
                        // certain is that this download needs a fresh link.
                        Err(_) => return Err(Error::Expired(code)),
                    }
                }
                other => return other,
            }
        }
    }

    async fn open_at(&self, target: &Target, start: u64, end: u64, total: u64) -> Result<HttpBody> {
        debug_assert!(start < end && end <= total);
        let mut request = self
            .build(&target.url)
            .header(header::RANGE, format!("bytes={}-{}", start, end - 1));
        // `If-Range` makes the server itself refuse to mix two versions of a
        // file. Only a strong ETag is valid there.
        if let Some(etag) = target.validators.etag.as_deref().filter(|e| !e.starts_with("W/")) {
            request = request.header(header::IF_RANGE, etag);
        }
        let response = request.send().await?;
        let headers = response.headers();

        match response.status() {
            StatusCode::PARTIAL_CONTENT => {
                let range = content_range(headers)
                    .ok_or_else(|| Error::Protocol("the server sent a partial response without saying which part".into()))?;
                if range.first != start {
                    return Err(Error::Protocol(format!(
                        "asked for byte {start}, the server answered from byte {}",
                        range.first
                    )));
                }
                if range.total.is_some_and(|t| t != total) {
                    return Err(Error::Changed);
                }
                let seen = Validators {
                    etag: header_str(headers, header::ETAG),
                    last_modified: header_str(headers, header::LAST_MODIFIED),
                };
                if target.validators.conflicts_with(&seen) {
                    return Err(Error::Changed);
                }
                Ok(HttpBody { response })
            }
            // The whole file from byte zero is still the right bytes for a
            // range that starts at zero.
            StatusCode::OK if start == 0 => {
                if response.content_length().is_some_and(|len| len != total) {
                    return Err(Error::Changed);
                }
                Ok(HttpBody { response })
            }
            // `If-Range` failed: the server is sending a different version.
            StatusCode::OK if target.validators.etag.is_some() => Err(Error::Changed),
            StatusCode::OK => Err(Error::RangeIgnored),
            StatusCode::RANGE_NOT_SATISFIABLE => Err(Error::Changed),
            _ => Err(status_error(&response)),
        }
    }

    /// Follows the original link again after the pinned target expired.
    async fn resolve_again(&self, seen_generation: u64, total: u64) -> Result<()> {
        let _guard = self.resolving.lock().await;
        if self.target.read().generation != seen_generation {
            // Another worker already did it while this one waited.
            return Ok(());
        }
        let before = self.target.read().clone();
        let (probe, body) = self.probe().await?;
        drop(body);

        let mismatch = if probe.total != Some(total) {
            Some(Error::Mismatch(match probe.total {
                Some(found) => format!("it is {found} bytes, this download is {total}"),
                None => "it does not report a size".into(),
            }))
        } else if same_resource(&before.url, &probe.final_url) && before.validators.conflicts_with(&probe.validators) {
            Some(Error::Changed)
        } else {
            None
        };
        if let Some(e) = mismatch {
            // Keep pointing at the dead target rather than at a different file.
            let mut target = self.target.write();
            target.url = before.url;
            target.validators = before.validators;
            return Err(e);
        }
        Ok(())
    }

    /// The URL ranged requests currently go to (the redirect target).
    pub fn target_url(&self) -> String {
        self.target.read().url.clone()
    }

    /// Validators of the URL ranged requests currently go to.
    pub fn validators(&self) -> Validators {
        self.target.read().validators.clone()
    }

    /// Restores the pinned validators from a checkpoint, when the resumed
    /// download is still being served from the same place.
    pub fn expect(&self, validators: Validators) {
        self.target.write().validators = validators;
    }

    fn build(&self, url: &str) -> RequestBuilder {
        let cross_origin = !same_origin(url, &self.request.url);
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, HeaderValue::from_static("*/*"));
        // Compression would make byte offsets meaningless.
        headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        if let Some(referer) = self.request.referer.as_deref()
            && let Ok(value) = HeaderValue::from_str(referer)
        {
            headers.insert(header::REFERER, value);
        }
        for (name, value) in &self.request.headers {
            let lower = name.trim().to_ascii_lowercase();
            if BLOCKED_HEADERS.contains(&lower.as_str())
                || lower.starts_with("proxy-")
                || lower.starts_with(':')
                || (cross_origin && ORIGIN_BOUND_HEADERS.contains(&lower.as_str()))
            {
                continue;
            }
            if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(lower.as_bytes()), HeaderValue::from_str(value.trim())) {
                headers.insert(name, value);
            }
        }
        self.client.get(url).headers(headers)
    }
}

fn status_error(response: &Response) -> Error {
    let retry_after = response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_retry_after);
    Error::from_status(response.status().as_u16(), retry_after)
}

/// `Retry-After` is either a number of seconds or an HTTP date.
fn parse_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let when = httpdate::parse_http_date(value).ok()?;
    Some(when.duration_since(std::time::SystemTime::now()).unwrap_or(Duration::ZERO))
}

fn header_str(headers: &HeaderMap, name: HeaderName) -> Option<String> {
    headers
        .get(name)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).trim().to_owned())
        .filter(|s| !s.is_empty())
}

#[derive(Debug, PartialEq, Eq)]
struct ContentRange {
    first: u64,
    last: u64,
    total: Option<u64>,
}

/// Parses `Content-Range: bytes 0-0/1234` (and `bytes 0-0/*`).
fn content_range(headers: &HeaderMap) -> Option<ContentRange> {
    parse_content_range(headers.get(header::CONTENT_RANGE)?.to_str().ok()?)
}

fn parse_content_range(value: &str) -> Option<ContentRange> {
    let value = value.trim();
    let rest = value
        .get(..5)
        .filter(|unit| unit.eq_ignore_ascii_case("bytes"))
        .map(|_| value[5..].trim_start_matches([' ', '=']))?;
    let (range, total) = rest.split_once('/')?;
    let (first, last) = range.trim().split_once('-')?;
    let first = first.trim().parse().ok()?;
    let last = last.trim().parse().ok()?;
    let total = match total.trim() {
        "*" => None,
        t => Some(t.parse().ok()?),
    };
    (first <= last).then_some(ContentRange { first, last, total })
}

fn same_origin(a: &str, b: &str) -> bool {
    match (url::Url::parse(a), url::Url::parse(b)) {
        (Ok(a), Ok(b)) => a.origin() == b.origin(),
        _ => false,
    }
}

/// Two URLs name the same resource when they differ only in their query,
/// which is where signatures and tokens live.
pub(crate) fn same_resource(a: &str, b: &str) -> bool {
    fn key(u: &str) -> Option<String> {
        let mut u = url::Url::parse(u).ok()?;
        u.set_query(None);
        u.set_fragment(None);
        Some(u.to_string())
    }
    match (key(a), key(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_parsing() {
        assert_eq!(parse_content_range("bytes 0-0/1234"), Some(ContentRange { first: 0, last: 0, total: Some(1234) }));
        assert_eq!(parse_content_range("Bytes 5-9/*"), Some(ContentRange { first: 5, last: 9, total: None }));
        assert_eq!(parse_content_range("bytes=10-19/100"), Some(ContentRange { first: 10, last: 19, total: Some(100) }));
        assert_eq!(parse_content_range("bytes */0"), None);
        assert_eq!(parse_content_range("items 0-1/2"), None);
        assert_eq!(parse_content_range("bytes 9-5/20"), None);
        assert_eq!(parse_content_range(""), None);
    }

    #[test]
    fn retry_after_forms() {
        assert_eq!(parse_retry_after("7"), Some(Duration::from_secs(7)));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), Some(Duration::ZERO));
        assert_eq!(parse_retry_after("soon"), None);
    }

    #[test]
    fn resource_identity_ignores_the_signature() {
        assert!(same_resource("https://cdn.example/a/b.zip?sig=1&exp=2", "https://cdn.example/a/b.zip?sig=9"));
        assert!(!same_resource("https://cdn1.example/a/b.zip", "https://cdn2.example/a/b.zip"));
        assert!(same_origin("https://a.example/x", "https://a.example:443/y"));
        assert!(!same_origin("https://a.example/x", "http://a.example/x"));
    }

    #[test]
    fn rejects_non_http_schemes() {
        let client = reqwest::Client::new();
        assert!(HttpSource::new(client.clone(), Request::new("file:///etc/passwd")).is_err());
        assert!(HttpSource::new(client.clone(), Request::new("not a url")).is_err());
        assert!(HttpSource::new(client, Request::new("  https://a.example/x  ")).is_ok());
    }
}
