//! Disk layer: pre-allocated data file, lock-free positional writes and the
//! atomically replaced resume sidecar.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::types::SegmentSnapshot;

/// Bumped when the sidecar layout changes incompatibly.
const META_VERSION: u32 = 1;

/// The in-progress data file.
///
/// Workers share one handle through an `Arc` and write with OS positional
/// writes (`pwrite` / `WriteFile` with an explicit offset), which neither
/// touch nor depend on a shared cursor. There is no mutex anywhere on the
/// write path; ranges are disjoint by construction of the segment map.
#[derive(Debug)]
pub struct PartFile {
    file: File,
    path: PathBuf,
}

impl PartFile {
    /// Creates the data file and reserves its full size up front.
    ///
    /// Reserving first means a full disk is reported before the first byte is
    /// fetched instead of at 97%, and the filesystem can lay the file out
    /// contiguously instead of interleaving it with whatever else is written
    /// meanwhile.
    pub fn create(path: &Path, total: Option<u64>) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if let Some(total) = total {
            ensure_space(path, total)?;
        }
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)?;
        let part = Self { file, path: path.to_owned() };
        if let Some(total) = total {
            part.reserve(total)?;
        }
        Ok(part)
    }

    /// Reopens a partial file for resume. The file must still have the exact
    /// size it was reserved with; anything else means it was tampered with.
    pub fn open_existing(path: &Path, total: u64) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let len = file.metadata()?.len();
        if len != total {
            return Err(Error::Disk(format!(
                "partial file is {len} bytes, expected {total}; it cannot be resumed"
            )));
        }
        Ok(Self { file, path: path.to_owned() })
    }

    fn reserve(&self, total: u64) -> Result<()> {
        // Real block reservation where the filesystem offers it. Some do not
        // (exFAT, most network shares); the logical length is enough there.
        let _ = fs4::FileExt::allocate(&self.file, total);
        if self.file.metadata()?.len() != total {
            self.file.set_len(total)?;
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes the whole buffer at an absolute offset. Safe to call from any
    /// number of threads at once on disjoint ranges.
    pub fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file.write_all_at(buf, offset)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            let mut written = 0usize;
            while written < buf.len() {
                match self.file.seek_write(&buf[written..], offset + written as u64) {
                    Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                    Ok(n) => written += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }
    }

    /// Reads exactly `buf.len()` bytes at an absolute offset.
    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file.read_exact_at(buf, offset)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            let mut read = 0usize;
            while read < buf.len() {
                match self.file.seek_read(&mut buf[read..], offset + read as u64) {
                    Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                    Ok(n) => read += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }
    }

    /// Flushes file contents to stable storage.
    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_data()
    }

    /// Sets the final length. Used when the size was unknown up front.
    pub fn set_len(&self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }
}

/// Fails early when the destination volume cannot hold the file.
pub fn ensure_space(path: &Path, needed: u64) -> Result<()> {
    let probe = path.parent().filter(|p| p.exists()).unwrap_or(Path::new("."));
    // A filesystem that cannot report its free space is not a reason to
    // refuse the download; the write itself will say so if it is full.
    if let Ok(available) = fs4::available_space(probe)
        && available < needed
    {
        return Err(Error::DiskFull { needed, available });
    }
    Ok(())
}

/// Everything needed to continue a download after a crash, a reboot or a
/// week-long pause. Written next to the data file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeMeta {
    pub version: u32,
    /// The URL the bytes were last fetched from. Informational: a refreshed
    /// link changes it without invalidating the byte map.
    pub url: String,
    pub total: u64,
    /// Validators captured on the first response. A mismatch on resume means
    /// the remote file is no longer the one these bytes came from.
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub segments: Vec<SegmentSnapshot>,
    pub saved_at: u64,
}

impl ResumeMeta {
    pub fn new(
        url: &str,
        total: u64,
        etag: Option<String>,
        last_modified: Option<String>,
        segments: Vec<SegmentSnapshot>,
    ) -> Self {
        Self {
            version: META_VERSION,
            url: url.to_owned(),
            total,
            etag,
            last_modified,
            segments,
            saved_at: crate::types::unix_now(),
        }
    }

    pub fn downloaded(&self) -> u64 {
        self.segments.iter().map(|s| s.done()).sum()
    }

    /// Loads a sidecar. A missing, truncated, foreign or structurally invalid
    /// file yields `None`: the caller starts over rather than trusting it.
    pub fn load(path: &Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        let meta: ResumeMeta = serde_json::from_slice(&bytes).ok()?;
        (meta.version == META_VERSION && crate::segments::is_partition(meta.total, &meta.segments))
            .then_some(meta)
    }

    /// Replaces the sidecar atomically: the new content is written and
    /// flushed under a temporary name, then renamed over the old file. A
    /// crash at any instant leaves either the old checkpoint or the new one,
    /// never a torn mix.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        write_atomic(path, &serde_json::to_vec(self).map_err(io::Error::other)?)
    }
}

/// Write-to-temp, flush, rename.
///
/// The temporary name is unique per call, so two writers racing on the same
/// target (a run being torn down while its successor starts) each rename a
/// complete file and the last one wins, instead of one pulling the temporary
/// file out from under the other.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(format!(".tmp.{}.{n}", std::process::id()));
    let tmp = PathBuf::from(tmp_name);
    let written = (|| {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_data()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Removes temporary files a crash may have left next to `path`.
pub fn remove_stale_temporaries(path: &Path) {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else { return };
    let prefix = format!("{}.tmp.", name.to_string_lossy());
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Moves a file, falling back to copy-and-delete across volumes.
pub fn move_file(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            std::fs::copy(from, to)?;
            // The copy is complete and durable before the source disappears.
            File::open(to)?.sync_all()?;
            std::fs::remove_file(from)
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("siphon-disk-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reserves_the_full_size_up_front() {
        let dir = scratch("reserve");
        let part = PartFile::create(&dir.join("a.bin.siphon"), Some(5_000_000)).unwrap();
        assert_eq!(std::fs::metadata(part.path()).unwrap().len(), 5_000_000);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn parallel_positional_writes_land_where_they_should() {
        let dir = scratch("pwrite");
        const BLOCK: usize = 64 * 1024;
        const BLOCKS: usize = 96;
        let part = Arc::new(PartFile::create(&dir.join("b.bin.siphon"), Some((BLOCK * BLOCKS) as u64)).unwrap());

        // Twelve threads write interleaved blocks, deliberately out of order.
        let threads: Vec<_> = (0..12)
            .map(|t| {
                let part = part.clone();
                std::thread::spawn(move || {
                    for block in (0..BLOCKS).rev().filter(|b| b % 12 == t) {
                        let buf = vec![block as u8; BLOCK];
                        part.write_at(&buf, (block * BLOCK) as u64).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        part.sync().unwrap();

        let data = std::fs::read(part.path()).unwrap();
        for (block, chunk) in data.chunks(BLOCK).enumerate() {
            assert!(chunk.iter().all(|&b| b == block as u8), "block {block} is corrupt");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resume_refuses_a_file_of_the_wrong_size() {
        let dir = scratch("size");
        let path = dir.join("c.bin.siphon");
        PartFile::create(&path, Some(1000)).unwrap();
        assert!(PartFile::open_existing(&path, 1000).is_ok());
        assert!(PartFile::open_existing(&path, 999).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sidecar_round_trips_and_rejects_garbage() {
        let dir = scratch("meta");
        let path = dir.join("d.bin.siphon.meta");
        let segs = vec![
            SegmentSnapshot { start: 0, pos: 40, end: 50, active: true },
            SegmentSnapshot { start: 50, pos: 50, end: 100, active: false },
        ];
        let meta = ResumeMeta::new("https://x.example/d.bin", 100, Some("\"abc\"".into()), None, segs);
        meta.save(&path).unwrap();

        let loaded = ResumeMeta::load(&path).unwrap();
        assert_eq!(loaded.total, 100);
        assert_eq!(loaded.downloaded(), 40);
        assert_eq!(loaded.etag.as_deref(), Some("\"abc\""));
        assert!(loaded.segments.iter().all(|s| !s.active), "ownership is never persisted");
        let leftovers = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(leftovers, 1, "no temporary file may outlive a save");

        // A crash can strand a temporary file; cleaning up removes only those.
        std::fs::write(dir.join("d.bin.siphon.meta.tmp.1.2"), b"x").unwrap();
        remove_stale_temporaries(&path);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        assert!(path.exists());

        // Torn write.
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
        assert!(ResumeMeta::load(&path).is_none());

        // Valid JSON, impossible byte map.
        let mut bad = meta.clone();
        bad.segments[1].start = 60;
        std::fs::write(&path, serde_json::to_vec(&bad).unwrap()).unwrap();
        assert!(ResumeMeta::load(&path).is_none());

        assert!(ResumeMeta::load(&dir.join("missing")).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refuses_a_file_larger_than_the_volume() {
        let dir = scratch("full");
        let err = PartFile::create(&dir.join("e.bin.siphon"), Some(u64::MAX / 2)).unwrap_err();
        assert!(matches!(err, Error::DiskFull { .. }), "{err:?}");
        assert!(!dir.join("e.bin.siphon").exists(), "nothing is created when space is short");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
