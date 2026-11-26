//! File checksums, for comparing a download with the value its publisher lists.

use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum HashAlgorithm {
    Md5,
    Sha1,
    #[default]
    Sha256,
}

impl HashAlgorithm {
    pub const ALL: [HashAlgorithm; 3] = [HashAlgorithm::Sha256, HashAlgorithm::Sha1, HashAlgorithm::Md5];

    pub fn label(self) -> &'static str {
        match self {
            HashAlgorithm::Md5 => "MD5",
            HashAlgorithm::Sha1 => "SHA-1",
            HashAlgorithm::Sha256 => "SHA-256",
        }
    }

    /// Length of the digest in hexadecimal characters.
    pub fn hex_len(self) -> usize {
        match self {
            HashAlgorithm::Md5 => 32,
            HashAlgorithm::Sha1 => 40,
            HashAlgorithm::Sha256 => 64,
        }
    }

    /// The algorithm a pasted checksum most likely belongs to, going by its
    /// length.
    pub fn guess(expected: &str) -> Option<HashAlgorithm> {
        let hex = normalize(expected);
        HashAlgorithm::ALL.into_iter().find(|a| a.hex_len() == hex.len())
    }
}

/// Reduces a checksum as people paste it (`SHA256: AB CD…`, `abcd…  file.iso`)
/// to bare lower-case hexadecimal. Returns an empty string when the text does
/// not contain one.
pub fn normalize(text: &str) -> String {
    let text = text.trim();
    // `sha256sum` output: the digest, two spaces, the file name.
    let candidates = text.split(|c: char| c.is_whitespace() || matches!(c, ':' | '=' | '(' | ')')).map(str::trim);
    let whole: String = text.chars().filter(|c| !c.is_whitespace() && !matches!(c, ':' | '-')).collect();
    candidates
        .chain(std::iter::once(whole.as_str()))
        .filter(|t| matches!(t.len(), 32 | 40 | 64) && t.bytes().all(|b| b.is_ascii_hexdigit()))
        .max_by_key(|t| t.len())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default()
}

/// Whether a computed digest equals what the user pasted.
pub fn matches(computed: &str, expected: &str) -> bool {
    let expected = normalize(expected);
    !expected.is_empty() && computed.eq_ignore_ascii_case(&expected)
}

fn digest<D: Digest>(mut file: std::fs::File, done: &AtomicU64, cancel: &AtomicBool) -> std::io::Result<Option<String>> {
    let mut hasher = D::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let n = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        hasher.update(&buffer[..n]);
        done.fetch_add(n as u64, Ordering::Relaxed);
    }
    Ok(Some(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()))
}

/// Hashes a file, reporting bytes read through `done`. Returns `None` when
/// `cancel` was raised before the end.
pub fn hash_file(path: &Path, algorithm: HashAlgorithm, done: &AtomicU64, cancel: &AtomicBool) -> std::io::Result<Option<String>> {
    let file = std::fs::File::open(path)?;
    match algorithm {
        HashAlgorithm::Md5 => digest::<Md5>(file, done, cancel),
        HashAlgorithm::Sha1 => digest::<Sha1>(file, done, cancel),
        HashAlgorithm::Sha256 => digest::<Sha256>(file, done, cancel),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(bytes: &[u8], algorithm: HashAlgorithm) -> String {
        let path = std::env::temp_dir().join(format!("siphon-hash-{}-{:?}-{}", std::process::id(), algorithm, bytes.len()));
        std::fs::write(&path, bytes).unwrap();
        let done = AtomicU64::new(0);
        let out = hash_file(&path, algorithm, &done, &AtomicBool::new(false)).unwrap().unwrap();
        assert_eq!(done.load(Ordering::Relaxed), bytes.len() as u64);
        std::fs::remove_file(path).unwrap();
        out
    }

    #[test]
    fn known_answers() {
        // The standard test vectors for "abc" and for the empty input.
        assert_eq!(hash(b"abc", HashAlgorithm::Md5), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(hash(b"abc", HashAlgorithm::Sha1), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(hash(b"abc", HashAlgorithm::Sha256), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(hash(b"", HashAlgorithm::Sha256), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(hash(b"", HashAlgorithm::Md5), "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn larger_than_one_buffer() {
        // One million 'a': the long SHA-256 test vector.
        let bytes = vec![b'a'; 1_000_000];
        assert_eq!(hash(&bytes, HashAlgorithm::Sha256), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
        let three = vec![b'a'; 3 * 1024 * 1024 + 17];
        assert_eq!(hash(&three, HashAlgorithm::Sha1).len(), 40);
    }

    #[test]
    fn cancelling_stops_without_a_result() {
        let path = std::env::temp_dir().join(format!("siphon-hash-cancel-{}", std::process::id()));
        std::fs::write(&path, vec![0u8; 4096]).unwrap();
        let out = hash_file(&path, HashAlgorithm::Sha256, &AtomicU64::new(0), &AtomicBool::new(true)).unwrap();
        assert_eq!(out, None);
        std::fs::remove_file(path).unwrap();
        assert!(hash_file(Path::new("/nonexistent/siphon"), HashAlgorithm::Md5, &AtomicU64::new(0), &AtomicBool::new(false)).is_err());
    }

    #[test]
    fn pasted_checksums_are_understood() {
        let sha = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(normalize(&sha.to_uppercase()), sha);
        assert_eq!(normalize(&format!("SHA256: {sha}")), sha);
        assert_eq!(normalize(&format!("{sha}  ubuntu.iso")), sha);
        assert_eq!(normalize(&format!("SHA256 (ubuntu.iso) = {sha}")), sha);
        assert_eq!(normalize("90 01 50 98 3c d2 4f b0 d6 96 3f 7d 28 e1 7f 72"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(normalize("not a checksum"), "");
        assert_eq!(normalize("abc123"), "");

        assert!(matches(sha, &format!("  {}  ", sha.to_uppercase())));
        assert!(!matches(sha, "900150983cd24fb0d6963f7d28e17f72"));
        assert!(!matches(sha, ""));
        assert_eq!(HashAlgorithm::guess(sha), Some(HashAlgorithm::Sha256));
        assert_eq!(HashAlgorithm::guess("900150983cd24fb0d6963f7d28e17f72"), Some(HashAlgorithm::Md5));
        assert_eq!(HashAlgorithm::guess("a9993e364706816aba3e25717850c26c9cd0d89d"), Some(HashAlgorithm::Sha1));
        assert_eq!(HashAlgorithm::guess("zz"), None);
    }
}
