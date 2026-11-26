//! Looks inside a remote ZIP archive without downloading it.
//!
//! A ZIP file keeps its table of contents at the end. Two small ranged
//! requests (the tail, then the central directory it points to) are enough
//! to list every entry of an archive of any size.

use crate::error::{Error, Result};
use crate::http::HttpSource;

const END_SIG: u32 = 0x0605_4b50;
const END64_LOCATOR_SIG: u32 = 0x0706_4b50;
const END64_SIG: u32 = 0x0606_4b50;
const CENTRAL_SIG: u32 = 0x0201_4b50;

const END_LEN: usize = 22;
const LOCATOR_LEN: usize = 20;
const END64_LEN: usize = 56;
const CENTRAL_LEN: usize = 46;
/// End record, the longest possible comment, and room for the ZIP64 records
/// that sit just before it.
const TAIL: u64 = (END_LEN + 65_535 + LOCATOR_LEN + END64_LEN) as u64;
/// A directory larger than this is listed only as far as it fits.
const MAX_DIRECTORY: u64 = 16 * 1024 * 1024;
const MAX_ENTRIES: usize = 100_000;

/// Extensions that are ZIP containers under another name.
const ZIP_EXTENSIONS: [&str; 14] = ["zip", "jar", "apk", "ipa", "epub", "docx", "xlsx", "pptx", "odt", "ods", "odp", "cbz", "whl", "xpi"];

/// Whether a file name suggests a ZIP container.
pub fn looks_like_zip(file_name: &str) -> bool {
    file_name.rsplit_once('.').is_some_and(|(_, ext)| ZIP_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveEntry {
    pub name: String,
    /// Size once unpacked.
    pub size: u64,
    pub compressed: u64,
    pub is_dir: bool,
    pub encrypted: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArchiveListing {
    pub entries: Vec<ArchiveEntry>,
    /// Number of files (folders excluded) the archive says it holds.
    pub files: usize,
    /// Total size of the listed entries once unpacked.
    pub unpacked: u64,
    /// The directory was too large to list in full.
    pub truncated: bool,
}

/// Where the central directory is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Directory {
    offset: u64,
    size: u64,
    entries: u64,
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

fn not_a_zip() -> Error {
    Error::Protocol("this does not look like a ZIP archive".into())
}

/// What the end of the file says.
#[derive(Debug, PartialEq, Eq)]
enum End {
    Classic(Directory),
    /// The real numbers are in a ZIP64 record at this offset.
    Zip64 { record: u64 },
}

/// Finds the end-of-central-directory record in the last bytes of the file.
/// `tail_start` is the file offset of `tail[0]`.
fn find_end(tail: &[u8], tail_start: u64) -> Result<End> {
    if tail.len() < END_LEN {
        return Err(not_a_zip());
    }
    // The record is followed only by its comment, whose length it states.
    // Searching backwards finds the real one even if the comment happens to
    // contain the signature.
    let at = (0..=tail.len() - END_LEN)
        .rev()
        .find(|&i| u32_at(tail, i) == Some(END_SIG) && u16_at(tail, i + 20).is_some_and(|comment| i + END_LEN + comment as usize == tail.len()))
        .ok_or_else(not_a_zip)?;

    let entries = u16_at(tail, at + 10).ok_or_else(not_a_zip)?;
    let size = u32_at(tail, at + 12).ok_or_else(not_a_zip)?;
    let offset = u32_at(tail, at + 16).ok_or_else(not_a_zip)?;
    let saturated = entries == u16::MAX || size == u32::MAX || offset == u32::MAX;
    if saturated
        && at >= LOCATOR_LEN
        && u32_at(tail, at - LOCATOR_LEN) == Some(END64_LOCATOR_SIG)
        && let Some(record) = u64_at(tail, at - LOCATOR_LEN + 8)
    {
        return Ok(End::Zip64 { record });
    }
    let _ = tail_start;
    Ok(End::Classic(Directory { offset: u64::from(offset), size: u64::from(size), entries: u64::from(entries) }))
}

fn parse_end64(record: &[u8]) -> Result<Directory> {
    if u32_at(record, 0) != Some(END64_SIG) {
        return Err(not_a_zip());
    }
    Ok(Directory {
        entries: u64_at(record, 32).ok_or_else(not_a_zip)?,
        size: u64_at(record, 40).ok_or_else(not_a_zip)?,
        offset: u64_at(record, 48).ok_or_else(not_a_zip)?,
    })
}

/// Names not flagged as UTF-8 are in the DOS code page. Its ASCII half is
/// the same; the upper half is mapped through CP437.
fn decode_name(bytes: &[u8], utf8: bool) -> String {
    if utf8 || bytes.is_ascii() {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    // Many tools write UTF-8 without setting the flag.
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_owned();
    }
    const HIGH: &str = "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{00A0}";
    let high: Vec<char> = HIGH.chars().collect();
    bytes.iter().map(|&b| if b < 0x80 { b as char } else { high[(b - 0x80) as usize] }).collect()
}

/// Parses central directory records until the bytes run out.
fn parse_directory(bytes: &[u8]) -> Vec<ArchiveEntry> {
    let mut entries = Vec::new();
    let mut at = 0usize;
    while entries.len() < MAX_ENTRIES && u32_at(bytes, at) == Some(CENTRAL_SIG) {
        let (Some(flags), Some(compressed), Some(size), Some(name_len), Some(extra_len), Some(comment_len)) =
            (u16_at(bytes, at + 8), u32_at(bytes, at + 20), u32_at(bytes, at + 24), u16_at(bytes, at + 28), u16_at(bytes, at + 30), u16_at(bytes, at + 32))
        else {
            break;
        };
        let name_at = at + CENTRAL_LEN;
        let extra_at = name_at + name_len as usize;
        let next = extra_at + extra_len as usize + comment_len as usize;
        let (Some(name), Some(extra)) = (bytes.get(name_at..extra_at), bytes.get(extra_at..extra_at + extra_len as usize)) else { break };

        let (mut size, mut compressed) = (u64::from(size), u64::from(compressed));
        // ZIP64: sizes that do not fit in 32 bits are in an extra field, in
        // this order, each present only if its 32-bit slot is saturated.
        let mut field = 0usize;
        while let (Some(id), Some(len)) = (u16_at(extra, field), u16_at(extra, field + 2)) {
            let data = field + 4;
            if id == 0x0001 {
                let mut cursor = data;
                if size == u64::from(u32::MAX)
                    && let Some(real) = u64_at(extra, cursor)
                {
                    size = real;
                    cursor += 8;
                }
                if compressed == u64::from(u32::MAX)
                    && let Some(real) = u64_at(extra, cursor)
                {
                    compressed = real;
                }
                break;
            }
            field = data + len as usize;
        }

        let name = decode_name(name, flags & 0x0800 != 0);
        entries.push(ArchiveEntry { is_dir: name.ends_with('/') || name.ends_with('\\'), name, size, compressed, encrypted: flags & 0x0001 != 0 });
        at = next;
    }
    entries
}

async fn read(source: &HttpSource, start: u64, end: u64, total: u64) -> Result<Vec<u8>> {
    let mut body = source.open(start, end, total).await?;
    let want = (end - start) as usize;
    let mut out = Vec::with_capacity(want);
    while out.len() < want {
        match body.chunk().await? {
            Some(chunk) => out.extend_from_slice(&chunk[..chunk.len().min(want - out.len())]),
            None => break,
        }
    }
    if out.len() < want {
        return Err(Error::Truncated);
    }
    Ok(out)
}

/// Lists a ZIP archive of `total` bytes behind a probed source.
pub async fn list(source: &HttpSource, total: u64) -> Result<ArchiveListing> {
    if total < END_LEN as u64 {
        return Err(not_a_zip());
    }
    let tail_start = total.saturating_sub(TAIL);
    let tail = read(source, tail_start, total, total).await?;

    // Bytes of the file that may already be in hand.
    let slice = |offset: u64, len: u64| -> Option<&[u8]> {
        let from = usize::try_from(offset.checked_sub(tail_start)?).ok()?;
        tail.get(from..from.checked_add(usize::try_from(len).ok()?)?)
    };

    let directory = match find_end(&tail, tail_start)? {
        End::Classic(directory) => directory,
        End::Zip64 { record } => {
            if record.checked_add(END64_LEN as u64).is_none_or(|end| end > total) {
                return Err(not_a_zip());
            }
            match slice(record, END64_LEN as u64) {
                Some(bytes) => parse_end64(bytes)?,
                None => parse_end64(&read(source, record, record + END64_LEN as u64, total).await?)?,
            }
        }
    };
    if directory.offset.checked_add(directory.size).is_none_or(|end| end > total) {
        return Err(not_a_zip());
    }

    let wanted = directory.size.min(MAX_DIRECTORY);
    let entries = if wanted == 0 {
        Vec::new()
    } else {
        match slice(directory.offset, wanted) {
            Some(bytes) => parse_directory(bytes),
            None => parse_directory(&read(source, directory.offset, directory.offset + wanted, total).await?),
        }
    };

    Ok(ArchiveListing {
        files: entries.iter().filter(|e| !e.is_dir).count(),
        unpacked: entries.iter().map(|e| e.size).sum(),
        truncated: (entries.len() as u64) < directory.entries,
        entries,
    })
}

/// Builds small archives for tests, here and in the integration suite.
#[doc(hidden)]
pub mod fixture {
    /// A ZIP of stored (uncompressed) entries. With `zip64`, every size and
    /// offset that can be moved into ZIP64 structures is.
    pub fn build(entries: &[(&str, &[u8])], comment: &[u8], zip64: bool) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        let mut central: Vec<u8> = Vec::new();
        for (name, data) in entries {
            let offset = out.len() as u32;
            let flags: u16 = if name.is_ascii() { 0 } else { 0x0800 };
            // Local file header.
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&flags.to_le_bytes());
            out.extend_from_slice(&[0u8; 2 + 2 + 2 + 4]);
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);

            let extra: Vec<u8> = if zip64 {
                let mut e = Vec::new();
                e.extend_from_slice(&0x0001u16.to_le_bytes());
                e.extend_from_slice(&16u16.to_le_bytes());
                e.extend_from_slice(&(data.len() as u64).to_le_bytes());
                e.extend_from_slice(&(data.len() as u64).to_le_bytes());
                e
            } else {
                Vec::new()
            };
            let size32 = if zip64 { u32::MAX } else { data.len() as u32 };
            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&[20, 0, 20, 0]);
            central.extend_from_slice(&flags.to_le_bytes());
            central.extend_from_slice(&[0u8; 2 + 2 + 2 + 4]);
            central.extend_from_slice(&size32.to_le_bytes());
            central.extend_from_slice(&size32.to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&(extra.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0u8; 2 + 2 + 2 + 4]);
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
            central.extend_from_slice(&extra);
        }
        let directory_offset = out.len() as u64;
        out.extend_from_slice(&central);
        if zip64 {
            let record_offset = out.len() as u64;
            out.extend_from_slice(&0x0606_4b50u32.to_le_bytes());
            out.extend_from_slice(&44u64.to_le_bytes());
            out.extend_from_slice(&[45, 0, 45, 0]);
            out.extend_from_slice(&[0u8; 8]);
            out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
            out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
            out.extend_from_slice(&(central.len() as u64).to_le_bytes());
            out.extend_from_slice(&directory_offset.to_le_bytes());
            out.extend_from_slice(&0x0706_4b50u32.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&record_offset.to_le_bytes());
            out.extend_from_slice(&1u32.to_le_bytes());
        }
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0u8; 4]);
        let (count, size, offset) = if zip64 { (u16::MAX, u32::MAX, u32::MAX) } else { (entries.len() as u16, central.len() as u32, directory_offset as u32) };
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(&(comment.len() as u16).to_le_bytes());
        out.extend_from_slice(comment);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory_of(zip: &[u8]) -> Directory {
        match find_end(zip, 0).unwrap() {
            End::Classic(d) => d,
            End::Zip64 { record } => parse_end64(&zip[record as usize..]).unwrap(),
        }
    }

    fn entries_of(zip: &[u8]) -> Vec<ArchiveEntry> {
        let d = directory_of(zip);
        parse_directory(&zip[d.offset as usize..(d.offset + d.size) as usize])
    }

    #[test]
    fn lists_a_plain_archive() {
        let zip = fixture::build(&[("readme.txt", b"hello"), ("docs/", b""), ("docs/guide.pdf", &[7u8; 300])], b"", false);
        let entries = entries_of(&zip);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0], ArchiveEntry { name: "readme.txt".into(), size: 5, compressed: 5, is_dir: false, encrypted: false });
        assert!(entries[1].is_dir);
        assert_eq!(entries[2].size, 300);
        assert_eq!(directory_of(&zip).entries, 3);
    }

    #[test]
    fn a_comment_does_not_hide_the_end_record() {
        // The comment even contains the signature of the record itself.
        let mut comment = b"released ".to_vec();
        comment.extend_from_slice(&END_SIG.to_le_bytes());
        comment.extend_from_slice(b" trailing words");
        let zip = fixture::build(&[("a.bin", &[1u8; 10])], &comment, false);
        assert_eq!(entries_of(&zip).len(), 1);
    }

    #[test]
    fn zip64_sizes_come_from_the_extra_field() {
        let zip = fixture::build(&[("big.iso", &[9u8; 64]), ("ünï.txt", b"x")], b"", true);
        assert!(matches!(find_end(&zip, 0).unwrap(), End::Zip64 { .. }));
        let entries = entries_of(&zip);
        assert_eq!(entries.len(), 2);
        assert_eq!((entries[0].size, entries[0].compressed), (64, 64));
        assert_eq!(entries[1].name, "ünï.txt");
        assert_eq!(directory_of(&zip).entries, 2);
    }

    #[test]
    fn names_in_the_dos_code_page() {
        // "résumé.txt" in CP437: é is 0x82.
        assert_eq!(decode_name(b"r\x82sum\x82.txt", false), "résumé.txt");
        assert_eq!(decode_name("résumé.txt".as_bytes(), false), "résumé.txt");
        assert_eq!(decode_name(b"plain.txt", false), "plain.txt");
    }

    #[test]
    fn rejects_what_is_not_a_zip() {
        assert!(find_end(b"definitely not an archive, just some text", 0).is_err());
        assert!(find_end(b"short", 0).is_err());
        assert!(parse_end64(&[0u8; 56]).is_err());
        // A directory cut short yields the entries that are whole.
        let zip = fixture::build(&[("a", b"1"), ("b", b"2")], b"", false);
        let d = directory_of(&zip);
        let cut = &zip[d.offset as usize..(d.offset + d.size - 5) as usize];
        assert_eq!(parse_directory(cut).len(), 1);
    }

    #[test]
    fn recognises_zip_containers_by_name() {
        assert!(looks_like_zip("Archive.ZIP"));
        assert!(looks_like_zip("app-release.apk"));
        assert!(looks_like_zip("report.docx"));
        assert!(!looks_like_zip("movie.mkv"));
        assert!(!looks_like_zip("archive.tar.gz"));
        assert!(!looks_like_zip("zip"));
    }
}
