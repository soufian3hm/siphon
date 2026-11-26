//! Microsoft Media Server streams, over their HTTP transport (MMSH).
//!
//! `mms://` addresses were served two ways: a proprietary TCP protocol and
//! "MMS over HTTP", which every Windows Media server also speaks and which
//! players fall back to. This module implements the HTTP form (MS-WMSP):
//!
//! 1. A *describe* request returns the ASF header, wrapped in a `$H` frame.
//!    It tells how large a data packet is and which streams exist.
//! 2. A *play* request selects every stream and returns the header again
//!    followed by `$D` frames, one ASF data packet each, with trailing
//!    padding stripped to save bandwidth.
//!
//! Saving the stream means writing the header and then every packet padded
//! back to the fixed packet size, which yields a regular `.asf`/`.wmv` file.
//! The transport is strictly sequential, so these downloads cannot be
//! segmented or resumed.

use std::path::Path;
use std::sync::Arc;

use bytes::{Buf, BytesMut};
use reqwest::header::{self, HeaderMap, HeaderValue};
use tokio_util::sync::CancellationToken;

use crate::disk::PartFile;
use crate::error::{Error, Result};
use crate::filename;
use crate::rate::Limiter;
use crate::transfer::{Outcome, Progress, blocking, finalize};
use crate::types::Request;

const USER_AGENT: &str = "NSPlayer/7.10.0.3059";
const CLIENT_GUID: &str = "xClientGUID={c77e7400-738a-11d2-9add-0020af0a3278}";

const FRAME_HEADER: u16 = 0x4824; // "$H"
const FRAME_DATA: u16 = 0x4424; // "$D"
const FRAME_END: u16 = 0x4524; // "$E"
const FRAME_STREAM_CHANGE: u16 = 0x4324; // "$C"
const FRAME_METADATA: u16 = 0x4D24; // "$M"
const FRAME_PACKET_PAIR: u16 = 0x5024; // "$P"

const GUID_HEADER: [u8; 16] = [0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C];
const GUID_FILE_PROPERTIES: [u8; 16] = [0xA1, 0xDC, 0xAB, 0x8C, 0x47, 0xA9, 0xCF, 0x11, 0x8E, 0xE4, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65];
const GUID_STREAM_PROPERTIES: [u8; 16] = [0x91, 0x07, 0xDC, 0xB7, 0xB7, 0xA9, 0xCF, 0x11, 0x8E, 0xE6, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65];

/// A sane upper bound; real ASF packets are a few kilobytes.
const MAX_PACKET_LEN: usize = 256 * 1024;
const MAX_HEADER_LEN: usize = 4 * 1024 * 1024;

/// What the ASF header says about the stream.
#[derive(Clone, Debug, PartialEq, Eq)]
struct AsfInfo {
    packet_len: usize,
    stream_ids: Vec<u8>,
    /// Total file size, absent for live broadcasts.
    file_size: Option<u64>,
}

/// Summary for the confirmation dialog.
#[derive(Clone, Debug)]
pub struct MmsInfo {
    pub file_name: String,
    pub total: Option<u64>,
    pub url: String,
}

/// A described stream, ready to be played into a file.
pub struct Session {
    client: reqwest::Client,
    request: Request,
    http_url: String,
    asf: AsfInfo,
}

impl Session {
    pub fn total(&self) -> Option<u64> {
        self.asf.file_size
    }

    pub fn url(&self) -> &str {
        &self.http_url
    }

    pub fn file_name(&self) -> String {
        file_name_for(&self.request.url)
    }
}

/// `mms://host/path` is fetched as `http://host/path`.
fn http_url(url: &str) -> Result<String> {
    let trimmed = url.trim();
    let rest = ["mms://", "mmsh://", "mmst://", "mmsu://"]
        .iter()
        .find_map(|scheme| {
            trimmed.get(..scheme.len()).filter(|p| p.eq_ignore_ascii_case(scheme)).map(|_| &trimmed[scheme.len()..])
        })
        .ok_or_else(|| Error::InvalidUrl(format!("not an MMS address: {trimmed}")))?;
    let http = format!("http://{rest}");
    url::Url::parse(&http).map_err(|e| Error::InvalidUrl(format!("{trimmed}: {e}")))?;
    Ok(http)
}

fn file_name_for(url: &str) -> String {
    let name = http_url(url).ok().and_then(|u| filename::from_url(&u)).unwrap_or_else(|| "stream".into());
    let (_, ext) = filename::split_extension(&name);
    if matches!(ext.to_ascii_lowercase().as_str(), "asf" | "wmv" | "wma") {
        name
    } else {
        // Whatever the address calls it, what arrives is an ASF container.
        filename::sanitize(&format!("{name}.asf"))
    }
}

fn base_headers(request: &Request) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT, HeaderValue::from_static("*/*"));
    headers.insert(header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
    headers.insert(header::CONNECTION, HeaderValue::from_static("close"));
    if let Some(cookie) = request.header("cookie")
        && let Ok(value) = HeaderValue::from_str(cookie)
    {
        headers.insert(header::COOKIE, value);
    }
    headers
}

async fn send(client: &reqwest::Client, url: &str, headers: HeaderMap) -> Result<reqwest::Response> {
    let response = client.get(url).headers(headers).send().await?;
    if !response.status().is_success() {
        return Err(Error::from_status(response.status().as_u16(), None));
    }
    Ok(response)
}

/// Reads whole frames out of a response body.
struct Frames {
    response: reqwest::Response,
    buffer: BytesMut,
}

struct Frame {
    kind: u16,
    payload: bytes::Bytes,
}

impl Frames {
    fn new(response: reqwest::Response) -> Self {
        Self { response, buffer: BytesMut::new() }
    }

    /// Makes at least `n` bytes available. `Ok(false)` is a clean end of
    /// stream before any of them arrived.
    async fn fill(&mut self, n: usize, progress: Option<&Progress>) -> Result<bool> {
        while self.buffer.len() < n {
            match self.response.chunk().await? {
                Some(chunk) => {
                    if let Some(progress) = progress {
                        progress.add_net(chunk.len() as u64);
                    }
                    self.buffer.extend_from_slice(&chunk);
                }
                None if self.buffer.is_empty() => return Ok(false),
                None => return Err(Error::Truncated),
            }
        }
        Ok(true)
    }

    async fn next(&mut self, progress: Option<&Progress>) -> Result<Option<Frame>> {
        if !self.fill(4, progress).await? {
            return Ok(None);
        }
        let kind = u16::from_le_bytes([self.buffer[0], self.buffer[1]]);
        let length = u16::from_le_bytes([self.buffer[2], self.buffer[3]]) as usize;
        // The length counts a second, frame-specific header that precedes
        // the payload: sequence number, flags and a repeated length.
        let extension = match kind {
            FRAME_HEADER | FRAME_DATA => 8,
            FRAME_END | FRAME_STREAM_CHANGE => 4,
            FRAME_METADATA | FRAME_PACKET_PAIR => 0,
            other => {
                return Err(Error::Protocol(format!(
                    "this server did not answer with a Windows Media stream (unexpected frame 0x{other:04x})"
                )));
            }
        };
        if length < extension {
            return Err(Error::Protocol("malformed Windows Media frame".into()));
        }
        if !self.fill(4 + length, progress).await? {
            return Err(Error::Truncated);
        }
        self.buffer.advance(4 + extension);
        let payload = self.buffer.split_to(length - extension).freeze();
        Ok(Some(Frame { kind, payload }))
    }
}

/// Walks the ASF header object for the three facts the transport needs.
fn parse_asf_header(header: &[u8]) -> Result<AsfInfo> {
    let bad = || Error::Protocol("the stream's ASF header is malformed".into());
    if header.len() < 30 || header[..16] != GUID_HEADER {
        return Err(bad());
    }
    let header_len = u64::from_le_bytes(header[16..24].try_into().unwrap()) as usize;
    let end = header_len.min(header.len());

    let mut packet_len = 0usize;
    let mut file_size = None;
    let mut stream_ids = Vec::new();
    let mut at = 30usize;
    while at + 24 <= end {
        let guid = &header[at..at + 16];
        let size = u64::from_le_bytes(header[at + 16..at + 24].try_into().unwrap()) as usize;
        if size < 24 || at + size > end {
            return Err(bad());
        }
        let object = &header[at..at + size];
        if guid == GUID_FILE_PROPERTIES {
            if object.len() < 104 {
                return Err(bad());
            }
            let flags = u32::from_le_bytes(object[88..92].try_into().unwrap());
            let size = u64::from_le_bytes(object[40..48].try_into().unwrap());
            // Bit 0: broadcast. The size field is meaningless then.
            file_size = (flags & 1 == 0 && size > 0).then_some(size);
            packet_len = u32::from_le_bytes(object[96..100].try_into().unwrap()) as usize;
        } else if guid == GUID_STREAM_PROPERTIES {
            if object.len() < 74 {
                return Err(bad());
            }
            let flags = u16::from_le_bytes([object[72], object[73]]);
            stream_ids.push((flags & 0x7F) as u8);
        }
        at += size;
    }

    if packet_len == 0 || packet_len > MAX_PACKET_LEN {
        return Err(bad());
    }
    Ok(AsfInfo { packet_len, stream_ids, file_size })
}

async fn describe(client: &reqwest::Client, request: &Request, url: &str) -> Result<(AsfInfo, Vec<u8>)> {
    let mut headers = base_headers(request);
    headers.append(
        header::PRAGMA,
        HeaderValue::from_static(
            "no-cache,rate=1.000000,stream-time=0,stream-offset=0:0,request-context=1,max-duration=0",
        ),
    );
    headers.append(header::PRAGMA, HeaderValue::from_static(CLIENT_GUID));

    let mut frames = Frames::new(send(client, url, headers).await?);
    loop {
        match frames.next(None).await? {
            Some(frame) if frame.kind == FRAME_HEADER => {
                if frame.payload.len() > MAX_HEADER_LEN {
                    return Err(Error::Protocol("the stream header is implausibly large".into()));
                }
                let info = parse_asf_header(&frame.payload)?;
                return Ok((info, frame.payload.to_vec()));
            }
            Some(_) => continue,
            None => return Err(Error::Protocol("the server closed the stream before describing it".into())),
        }
    }
}

pub async fn probe(client: &reqwest::Client, request: &Request) -> Result<MmsInfo> {
    let url = http_url(&request.url)?;
    let (asf, _) = describe(client, request, &url).await?;
    Ok(MmsInfo { file_name: file_name_for(&request.url), total: asf.file_size, url })
}

pub async fn open(client: &reqwest::Client, request: &Request) -> Result<Session> {
    let url = http_url(&request.url)?;
    let (asf, _) = describe(client, request, &url).await?;
    Ok(Session { client: client.clone(), request: request.clone(), http_url: url, asf })
}

pub async fn run(
    session: Session,
    final_path: &Path,
    progress: &Arc<Progress>,
    limiter: &Arc<Limiter>,
    cancel: &CancellationToken,
) -> Result<Outcome> {
    let Session { client, request, http_url, asf } = session;

    let mut headers = base_headers(&request);
    let streams = asf.stream_ids.iter().map(|id| format!("ffff:{id}:0")).collect::<Vec<_>>().join(" ");
    for pragma in [
        "no-cache,rate=1.000000,stream-time=0,stream-offset=4294967295:4294967295,request-context=2,max-duration=0".to_owned(),
        "xPlayStrm=1".to_owned(),
        CLIENT_GUID.to_owned(),
        format!("stream-switch-count={}", asf.stream_ids.len()),
        format!("stream-switch-entry={streams}"),
    ] {
        let value = HeaderValue::from_str(&pragma).map_err(|_| Error::Protocol("bad stream selection".into()))?;
        headers.append(header::PRAGMA, value);
    }

    let response = tokio::select! {
        _ = cancel.cancelled() => return Err(Error::Cancelled),
        response = send(&client, &http_url, headers) => response?,
    };
    let mut frames = Frames::new(response);

    let part_path = filename::part_path(final_path);
    let meta_path = filename::meta_path(final_path);
    let part = blocking(|| PartFile::create(&part_path, None))?;
    progress.reset();
    progress.connection_opened();

    let mut at = 0u64;
    let mut header_written = false;
    let mut packet = vec![0u8; asf.packet_len];

    let result: Result<()> = loop {
        let frame = tokio::select! {
            _ = cancel.cancelled() => break Err(Error::Cancelled),
            frame = frames.next(Some(progress)) => frame,
        };
        let frame = match frame {
            Ok(Some(frame)) => frame,
            // Servers end an on-demand stream by closing the connection.
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        };
        limiter.take(frame.payload.len()).await;

        match frame.kind {
            FRAME_HEADER if !header_written => {
                if let Err(e) = blocking(|| part.write_at(&frame.payload, 0)) {
                    break Err(e.into());
                }
                at = frame.payload.len() as u64;
                header_written = true;
            }
            FRAME_DATA if header_written => {
                if frame.payload.len() > asf.packet_len {
                    break Err(Error::Protocol("the stream sent a packet larger than it announced".into()));
                }
                // The wire form drops trailing padding; the file form has it.
                packet[..frame.payload.len()].copy_from_slice(&frame.payload);
                packet[frame.payload.len()..].fill(0);
                if let Err(e) = blocking(|| part.write_at(&packet, at)) {
                    break Err(e.into());
                }
                at += asf.packet_len as u64;
            }
            // The server switches to different content; this file is done.
            FRAME_END | FRAME_STREAM_CHANGE if header_written => break Ok(()),
            _ => {}
        }
        progress.set_stream_bytes(at);
    };
    progress.connection_closed();
    result?;

    if !header_written || at == 0 {
        return Err(Error::Protocol("the server sent no media".into()));
    }
    blocking(|| part.set_len(at).and_then(|()| part.sync()))?;
    drop(part);
    let path = finalize(&part_path, final_path, &meta_path)?;
    Ok(Outcome::Completed { path, bytes: at })
}

#[cfg(test)]
pub(crate) mod testdata {
    use super::*;

    /// Builds a minimal but structurally valid ASF header for one stream.
    pub fn asf_header(packet_len: u32, file_size: u64, broadcast: bool) -> Vec<u8> {
        let mut file_props = Vec::new();
        file_props.extend_from_slice(&GUID_FILE_PROPERTIES);
        file_props.extend_from_slice(&104u64.to_le_bytes());
        file_props.extend_from_slice(&[0xAB; 16]); // file id
        file_props.extend_from_slice(&file_size.to_le_bytes());
        file_props.extend_from_slice(&[0u8; 8 * 5]); // creation, packets, play, send, preroll
        file_props.extend_from_slice(&(if broadcast { 1u32 } else { 2u32 }).to_le_bytes());
        file_props.extend_from_slice(&packet_len.to_le_bytes());
        file_props.extend_from_slice(&packet_len.to_le_bytes());
        file_props.extend_from_slice(&128_000u32.to_le_bytes());
        assert_eq!(file_props.len(), 104);

        let mut stream_props = Vec::new();
        stream_props.extend_from_slice(&GUID_STREAM_PROPERTIES);
        stream_props.extend_from_slice(&78u64.to_le_bytes());
        stream_props.extend_from_slice(&[0x11; 16]); // stream type
        stream_props.extend_from_slice(&[0x22; 16]); // error correction type
        stream_props.extend_from_slice(&[0u8; 8 + 4 + 4]);
        stream_props.extend_from_slice(&1u16.to_le_bytes()); // stream number 1
        stream_props.extend_from_slice(&[0u8; 4]);
        assert_eq!(stream_props.len(), 78);

        let body_len = 30 + file_props.len() + stream_props.len();
        let mut header = Vec::new();
        header.extend_from_slice(&GUID_HEADER);
        header.extend_from_slice(&(body_len as u64).to_le_bytes());
        header.extend_from_slice(&2u32.to_le_bytes());
        header.extend_from_slice(&[1, 2]);
        header.extend_from_slice(&file_props);
        header.extend_from_slice(&stream_props);
        // The 50-byte data object header travels with the ASF header.
        header.extend_from_slice(&[0xDA; 50]);
        header
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_mapping() {
        assert_eq!(http_url("mms://media.example/shows/ep1.wmv").unwrap(), "http://media.example/shows/ep1.wmv");
        assert_eq!(http_url("MMSH://media.example:8080/a?b=1").unwrap(), "http://media.example:8080/a?b=1");
        assert!(http_url("http://media.example/a").is_err());
        assert_eq!(file_name_for("mms://media.example/shows/ep1.wmv"), "ep1.wmv");
        assert_eq!(file_name_for("mms://media.example/live"), "live.asf");
        assert_eq!(file_name_for("mms://media.example/"), "stream.asf");
    }

    #[test]
    fn header_parsing() {
        let header = testdata::asf_header(1444, 9_000_000, false);
        let info = parse_asf_header(&header).unwrap();
        assert_eq!(info, AsfInfo { packet_len: 1444, stream_ids: vec![1], file_size: Some(9_000_000) });

        let live = parse_asf_header(&testdata::asf_header(1444, 0, true)).unwrap();
        assert_eq!(live.file_size, None);

        assert!(parse_asf_header(&header[..20]).is_err());
        let mut wrong = header.clone();
        wrong[0] ^= 0xFF;
        assert!(parse_asf_header(&wrong).is_err());
        // An object that claims to extend past the header.
        let mut overflow = header.clone();
        overflow[30 + 16] = 0xFF;
        overflow[30 + 17] = 0xFF;
        assert!(parse_asf_header(&overflow).is_err());
        assert!(parse_asf_header(&testdata::asf_header(0, 1, false)).is_err());
    }
}
