//! Dynamic file segmentation.
//!
//! The file is never cut into fixed chunks. It starts as one range owned by
//! one worker; every other worker *steals*: it finds the range with the most
//! bytes still to fetch, cuts the unfetched part in half, lowers the owner's
//! end to the cut and takes the second half. The same thing happens every
//! time a worker runs out of work, so connections migrate towards whatever is
//! left, and a slow connection is automatically relieved of its tail.
//!
//! A steal is not free: the owner's connection is abandoned at the cut and
//! whatever the server had already put on the wire past it is thrown away.
//! So a range is only split when it is big enough to be worth a connection
//! *and* its owner would otherwise need a while to finish it.
//!
//! # Concurrency contract
//!
//! * The segment list is guarded by one mutex, taken only when a worker asks
//!   for work or hands it back. It is never held across I/O.
//! * A segment's `pos` is written by its owner only; `end` is lowered by a
//!   stealer only, under the list mutex. Both are plain atomics, so the hot
//!   path (one check per network chunk) takes no lock.
//! * The segments always partition `[0, total)`. A stealer can lower `end`
//!   while the owner is mid-write; the owner then clamps to the new end and
//!   the stealer re-fetches from the cut. Any byte written twice is written
//!   with identical content at an identical offset, so the race costs at most
//!   one chunk of bandwidth and can never corrupt the file.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::types::SegmentSnapshot;

/// A range is only split when at least this much of it is still unfetched, so
/// each half is worth the cost of a new connection.
pub const DEFAULT_MIN_SPLIT: u64 = 1024 * 1024;

/// A range whose owner will finish it sooner than this is left alone.
pub const DEFAULT_MIN_STEAL_TIME: Duration = Duration::from_secs(2);

/// An owner needs this long on a range before its speed means anything.
const SPEED_WARMUP: Duration = Duration::from_millis(300);

/// When stealing is worth it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StealPolicy {
    /// Minimum unfetched bytes in a range for it to be split.
    pub min_split: u64,
    /// Minimum time the owner would still need, at its measured speed.
    pub min_time: Duration,
}

impl Default for StealPolicy {
    fn default() -> Self {
        Self { min_split: DEFAULT_MIN_SPLIT, min_time: DEFAULT_MIN_STEAL_TIME }
    }
}

const PENDING: u8 = 0;
const ACTIVE: u8 = 1;
const DONE: u8 = 2;

/// One contiguous byte range and its write cursor.
#[derive(Debug)]
pub struct Segment {
    start: u64,
    pos: AtomicU64,
    end: AtomicU64,
    state: AtomicU8,
    /// When the current owner took the range (microseconds since the map was
    /// created) and where the cursor was then. Together with `pos` they give
    /// the owner's average speed.
    owned_since: AtomicU64,
    owned_from: AtomicU64,
}

impl Segment {
    fn new(start: u64, pos: u64, end: u64, state: u8, now_micros: u64) -> Arc<Self> {
        debug_assert!(start <= pos && pos <= end);
        Arc::new(Self {
            start,
            pos: AtomicU64::new(pos),
            end: AtomicU64::new(end),
            state: AtomicU8::new(state),
            owned_since: AtomicU64::new(now_micros),
            owned_from: AtomicU64::new(pos),
        })
    }

    fn take_ownership(&self, now_micros: u64) {
        self.owned_since.store(now_micros, Ordering::Relaxed);
        self.owned_from.store(self.pos.load(Ordering::Acquire), Ordering::Relaxed);
        self.state.store(ACTIVE, Ordering::Release);
    }

    /// Time the owner still needs for `remaining` bytes at its average speed
    /// so far. `None` while there is not enough history, or when the owner
    /// has made no progress at all (in which case relieving it is welcome).
    fn time_left(&self, remaining: u64, now_micros: u64) -> Option<Duration> {
        let elapsed = now_micros.saturating_sub(self.owned_since.load(Ordering::Relaxed));
        if elapsed < SPEED_WARMUP.as_micros() as u64 {
            return None;
        }
        let done = self.pos.load(Ordering::Acquire).saturating_sub(self.owned_from.load(Ordering::Relaxed));
        if done == 0 {
            return None;
        }
        let micros = remaining as u128 * elapsed as u128 / done as u128;
        Some(Duration::from_micros(micros.min(u64::MAX as u128) as u64))
    }

    pub fn start(&self) -> u64 {
        self.start
    }

    /// Exclusive end. May shrink at any moment while the segment is active.
    pub fn end(&self) -> u64 {
        self.end.load(Ordering::Acquire)
    }

    /// Next byte to write, never reported past `end`.
    pub fn pos(&self) -> u64 {
        self.pos.load(Ordering::Acquire).min(self.end())
    }

    pub fn remaining(&self) -> u64 {
        let end = self.end();
        end.saturating_sub(self.pos.load(Ordering::Acquire).min(end))
    }

    /// How many bytes of a `len`-byte buffer starting at `at` still belong to
    /// this segment. Zero means a stealer has taken everything from `at` on.
    pub fn writable(&self, at: u64, len: usize) -> usize {
        self.end().saturating_sub(at).min(len as u64) as usize
    }

    /// Records that everything before `pos` is on disk. Owner only.
    pub fn commit(&self, pos: u64) {
        self.pos.store(pos, Ordering::Release);
    }

    fn snapshot(&self) -> SegmentSnapshot {
        let end = self.end();
        SegmentSnapshot {
            start: self.start,
            pos: self.pos.load(Ordering::Acquire).clamp(self.start, end),
            end,
            active: self.state.load(Ordering::Acquire) == ACTIVE,
        }
    }
}

/// The shared byte map of one download.
#[derive(Debug)]
pub struct SegmentMap {
    total: u64,
    policy: StealPolicy,
    epoch: Instant,
    segments: Mutex<Vec<Arc<Segment>>>,
}

/// Why a saved byte map was rejected.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum RestoreError {
    #[error("the saved byte map does not start at zero")]
    BadStart,
    #[error("the saved byte map has a gap or overlap at byte {0}")]
    NotContiguous(u64),
    #[error("the saved byte map has an inconsistent range at byte {0}")]
    BadRange(u64),
    #[error("the saved byte map covers {found} bytes, expected {expected}")]
    WrongTotal { expected: u64, found: u64 },
}

impl SegmentMap {
    /// A new download: one unowned range covering the whole file.
    pub fn fresh(total: u64) -> Self {
        Self::new(total, StealPolicy::default())
    }

    pub fn new(total: u64, policy: StealPolicy) -> Self {
        let state = if total == 0 { DONE } else { PENDING };
        Self {
            total,
            policy: StealPolicy { min_split: policy.min_split.max(2), ..policy },
            epoch: Instant::now(),
            segments: Mutex::new(vec![Segment::new(0, 0, total, state, 0)]),
        }
    }

    /// Rebuilds the map from a checkpoint, refusing anything that is not an
    /// exact partition of `[0, total)`. Unfinished ranges come back unowned.
    pub fn restore(total: u64, saved: &[SegmentSnapshot], policy: StealPolicy) -> Result<Self, RestoreError> {
        let mut sorted: Vec<SegmentSnapshot> = saved.iter().copied().filter(|s| !s.is_empty()).collect();
        sorted.sort_by_key(|s| s.start);

        if total == 0 {
            return Ok(Self::new(0, policy));
        }
        if sorted.first().map(|s| s.start) != Some(0) {
            return Err(RestoreError::BadStart);
        }

        let mut cursor = 0u64;
        let mut segments: Vec<Arc<Segment>> = Vec::with_capacity(sorted.len());
        for s in &sorted {
            if s.start != cursor {
                return Err(RestoreError::NotContiguous(cursor));
            }
            if s.pos < s.start || s.pos > s.end || s.end > total {
                return Err(RestoreError::BadRange(s.start));
            }
            cursor = s.end;

            let complete = s.pos == s.end;
            // Fold runs of finished ranges together so the map does not grow
            // without bound across many pause/resume cycles.
            if complete
                && let Some(prev) = segments.last()
                && prev.state.load(Ordering::Relaxed) == DONE
            {
                prev.end.store(s.end, Ordering::Relaxed);
                prev.pos.store(s.end, Ordering::Relaxed);
                continue;
            }
            segments.push(Segment::new(s.start, s.pos, s.end, if complete { DONE } else { PENDING }, 0));
        }
        if cursor != total {
            return Err(RestoreError::WrongTotal { expected: total, found: cursor });
        }

        let mut map = Self::new(total, policy);
        map.segments = Mutex::new(segments);
        Ok(map)
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    /// Hands out work: an unowned range if there is one (largest first),
    /// otherwise the second half of the busiest worker's remaining bytes.
    /// `None` means nothing is left that is worth another connection.
    pub fn acquire(&self) -> Option<Arc<Segment>> {
        self.acquire_at(Instant::now())
    }

    /// [`acquire`](Self::acquire) with an explicit clock, for tests.
    pub fn acquire_at(&self, now: Instant) -> Option<Arc<Segment>> {
        let now_micros = now.saturating_duration_since(self.epoch).as_micros() as u64;
        let mut segments = self.segments.lock();

        let pending = segments
            .iter()
            .filter(|s| s.state.load(Ordering::Acquire) == PENDING && s.remaining() > 0)
            .max_by_key(|s| s.remaining())
            .cloned();
        if let Some(seg) = pending {
            seg.take_ownership(now_micros);
            return Some(seg);
        }

        let (index, victim) = segments
            .iter()
            .enumerate()
            .filter(|(_, s)| s.state.load(Ordering::Acquire) == ACTIVE)
            .filter(|(_, s)| {
                let remaining = s.remaining();
                remaining >= self.policy.min_split
                    && s.time_left(remaining, now_micros).is_none_or(|left| left >= self.policy.min_time)
            })
            .max_by_key(|(_, s)| s.remaining())
            .map(|(i, s)| (i, s.clone()))?;

        let end = victim.end();
        let pos = victim.pos.load(Ordering::Acquire).min(end);
        let remaining = end - pos;
        if remaining < self.policy.min_split {
            return None;
        }

        let cut = pos + remaining / 2;
        victim.end.store(cut, Ordering::Release);
        let stolen = Segment::new(cut, cut, end, ACTIVE, now_micros);
        segments.insert(index + 1, stolen.clone());
        Some(stolen)
    }

    /// The owner is finished with a segment, successfully or not. A range
    /// with bytes left goes back to the pool for another worker.
    pub fn release(&self, segment: &Arc<Segment>) {
        let _guard = self.segments.lock();
        let next = if segment.remaining() == 0 { DONE } else { PENDING };
        segment.state.store(next, Ordering::Release);
    }

    /// Bytes safely on disk.
    pub fn downloaded(&self) -> u64 {
        self.segments.lock().iter().map(|s| s.pos() - s.start()).sum()
    }

    pub fn is_complete(&self) -> bool {
        self.segments.lock().iter().all(|s| s.remaining() == 0)
    }

    /// Ranges a worker currently owns.
    pub fn active_count(&self) -> usize {
        self.segments
            .lock()
            .iter()
            .filter(|s| s.state.load(Ordering::Acquire) == ACTIVE)
            .count()
    }

    /// A consistent, ordered copy of the map for checkpoints and the UI.
    ///
    /// Taken under the list mutex, so a concurrent steal is either entirely
    /// visible or not at all and the result always partitions the file.
    pub fn snapshot(&self) -> Vec<SegmentSnapshot> {
        self.segments.lock().iter().map(|s| s.snapshot()).collect()
    }
}

/// Checks that a snapshot is an exact partition of `[0, total)`.
pub fn is_partition(total: u64, snapshot: &[SegmentSnapshot]) -> bool {
    let mut cursor = 0;
    for s in snapshot {
        if s.start != cursor || s.pos < s.start || s.pos > s.end {
            return false;
        }
        cursor = s.end;
    }
    cursor == total
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    const P: StealPolicy = StealPolicy { min_split: DEFAULT_MIN_SPLIT, min_time: Duration::ZERO };

    fn snap(start: u64, pos: u64, end: u64) -> SegmentSnapshot {
        SegmentSnapshot { start, pos, end, active: false }
    }

    /// A policy that splits anything of at least `min_split` bytes, whatever the speed.
    fn tiny(min_split: u64) -> StealPolicy {
        StealPolicy { min_split, min_time: Duration::ZERO }
    }

    #[test]
    fn a_range_about_to_finish_is_left_alone() {
        let map = SegmentMap::new(100 * MIB, StealPolicy { min_split: MIB, min_time: Duration::from_secs(2) });
        let t0 = Instant::now();
        let a = map.acquire_at(t0).unwrap();

        // No history yet: split freely. This is how the first workers fan out.
        let b = map.acquire_at(t0).unwrap();
        assert_eq!((b.start(), b.end()), (50 * MIB, 100 * MIB));

        // One second in, `a` runs at 40 MiB/s with 10 MiB left (0.25 s) and
        // `b` at 10 MiB/s with 40 MiB left (4 s). Only `b` is worth relieving.
        a.commit(40 * MIB);
        b.commit(60 * MIB);
        let c = map.acquire_at(t0 + Duration::from_secs(1)).unwrap();
        assert_eq!((c.start(), c.end()), (80 * MIB, 100 * MIB));
        assert_eq!(a.end(), 50 * MIB, "the fast range was not touched");

        // Now everyone finishes within two seconds: nothing to steal, even
        // though every range is far above the size threshold.
        b.commit(75 * MIB);
        c.commit(95 * MIB);
        assert!(map.acquire_at(t0 + Duration::from_millis(1500)).is_none());

        // A stalled owner (no progress at all since it took the range) is
        // always fair game.
        a.commit(50 * MIB);
        b.commit(80 * MIB);
        map.release(&c);
        let c2 = map.acquire_at(t0 + Duration::from_secs(2)).unwrap();
        assert!(Arc::ptr_eq(&c, &c2));
        let d = map.acquire_at(t0 + Duration::from_secs(10)).unwrap();
        assert_eq!(d.start(), 97 * MIB + MIB / 2);
    }

    #[test]
    fn first_worker_takes_everything_then_others_halve_the_largest() {
        let map = SegmentMap::fresh(64 * MIB);
        let a = map.acquire().unwrap();
        assert_eq!((a.start(), a.end()), (0, 64 * MIB));

        let b = map.acquire().unwrap();
        assert_eq!((b.start(), b.end()), (32 * MIB, 64 * MIB));
        assert_eq!(a.end(), 32 * MIB, "the victim's end is lowered to the cut");

        let c = map.acquire().unwrap();
        let d = map.acquire().unwrap();
        let mut lens: Vec<u64> = [&a, &b, &c, &d].iter().map(|s| s.end() - s.start()).collect();
        lens.sort();
        assert_eq!(lens, vec![16 * MIB; 4]);
        assert!(is_partition(64 * MIB, &map.snapshot()));
        assert_eq!(map.active_count(), 4);
    }

    #[test]
    fn a_steal_splits_only_the_unfetched_part() {
        let map = SegmentMap::fresh(100 * MIB);
        let a = map.acquire().unwrap();
        a.commit(60 * MIB);
        let b = map.acquire().unwrap();
        assert_eq!(b.start(), 80 * MIB);
        assert_eq!(a.end(), 80 * MIB);
        assert_eq!(map.downloaded(), 60 * MIB);
    }

    #[test]
    fn small_remainders_are_not_split() {
        let map = SegmentMap::fresh(10 * MIB);
        let a = map.acquire().unwrap();
        a.commit(10 * MIB - 512 * 1024);
        assert!(map.acquire().is_none());
        assert_eq!(a.end(), 10 * MIB, "a refused steal must not move the end");
    }

    #[test]
    fn released_ranges_are_preferred_over_stealing() {
        let map = SegmentMap::fresh(64 * MIB);
        let a = map.acquire().unwrap();
        let b = map.acquire().unwrap();
        b.commit(b.start() + MIB);
        map.release(&b);

        let c = map.acquire().unwrap();
        assert!(Arc::ptr_eq(&b, &c), "the abandoned range is handed out whole");
        assert_eq!(c.pos(), 33 * MIB, "and resumes where it stopped");
        assert_eq!(a.end(), 32 * MIB);
    }

    #[test]
    fn the_owner_clamps_when_a_stealer_cuts_under_it() {
        let map = SegmentMap::fresh(8 * MIB);
        let a = map.acquire().unwrap();
        let chunk_at = MIB;
        // The owner has a 6 MiB buffer ready to write at 1 MiB when the steal lands.
        let b = map.acquire().unwrap();
        assert_eq!(b.start(), 4 * MIB);
        assert_eq!(a.writable(chunk_at, 6 * MIB as usize), 3 * MIB as usize);
        // Even if the owner had already written past the cut, its reported
        // position never exceeds its end, so the accounting stays exact.
        a.commit(7 * MIB);
        assert_eq!(a.pos(), 4 * MIB);
        assert_eq!(a.remaining(), 0);
        assert!(is_partition(8 * MIB, &map.snapshot()));
        assert_eq!(map.downloaded(), 4 * MIB);
    }

    #[test]
    fn completes_when_every_range_is_written() {
        let map = SegmentMap::fresh(4 * MIB);
        let a = map.acquire().unwrap();
        let b = map.acquire().unwrap();
        a.commit(a.end());
        map.release(&a);
        assert!(!map.is_complete());
        b.commit(b.end());
        map.release(&b);
        assert!(map.is_complete());
        assert!(map.acquire().is_none());
        assert_eq!(map.downloaded(), 4 * MIB);
    }

    #[test]
    fn empty_file_is_complete_immediately() {
        let map = SegmentMap::fresh(0);
        assert!(map.is_complete());
        assert!(map.acquire().is_none());
    }

    #[test]
    fn restore_round_trips_and_merges_finished_runs() {
        let saved = vec![
            snap(0, 10, 10),
            snap(10, 20, 20),
            snap(20, 25, 40),
            snap(40, 40, 70),
            snap(70, 100, 100),
        ];
        let map = SegmentMap::restore(100, &saved, tiny(4)).unwrap();
        let restored = map.snapshot();
        assert_eq!(
            restored,
            vec![snap(0, 20, 20), snap(20, 25, 40), snap(40, 40, 70), snap(70, 100, 100)]
        );
        assert_eq!(map.downloaded(), 55);

        // Largest unowned range first.
        let first = map.acquire().unwrap();
        assert_eq!((first.start(), first.pos(), first.end()), (40, 40, 70));
        let second = map.acquire().unwrap();
        assert_eq!((second.start(), second.pos(), second.end()), (20, 25, 40));
    }

    #[test]
    fn restore_rejects_corrupt_maps() {
        assert_eq!(SegmentMap::restore(100, &[snap(5, 5, 100)], P).unwrap_err(), RestoreError::BadStart);
        assert_eq!(
            SegmentMap::restore(100, &[snap(0, 0, 40), snap(50, 50, 100)], P).unwrap_err(),
            RestoreError::NotContiguous(40)
        );
        assert_eq!(
            SegmentMap::restore(100, &[snap(0, 0, 60), snap(40, 40, 100)], P).unwrap_err(),
            RestoreError::NotContiguous(60)
        );
        assert_eq!(
            SegmentMap::restore(100, &[snap(0, 0, 50)], P).unwrap_err(),
            RestoreError::WrongTotal { expected: 100, found: 50 }
        );
        assert_eq!(
            SegmentMap::restore(100, &[snap(0, 0, 50), snap(50, 49, 100)], P).unwrap_err(),
            RestoreError::BadRange(50)
        );
        assert_eq!(SegmentMap::restore(100, &[snap(0, 0, 120)], P).unwrap_err(), RestoreError::BadRange(0));
        assert_eq!(SegmentMap::restore(100, &[], P).unwrap_err(), RestoreError::BadStart);
    }

    /// Hammers the map from many threads, each simulating a worker that
    /// writes in chunks and re-checks its end like the real one does, and
    /// verifies that every byte is covered exactly by the final partition.
    #[test]
    fn concurrent_workers_cover_every_byte() {
        use std::sync::atomic::AtomicU8 as Cell;

        const TOTAL: u64 = 3_000_017;
        const CHUNK: usize = 4_099;
        let map = Arc::new(SegmentMap::new(TOTAL, tiny(16_384)));
        let written: Arc<Vec<Cell>> = Arc::new((0..TOTAL).map(|_| Cell::new(0)).collect());

        let workers: Vec<_> = (0..12)
            .map(|worker| {
                let map = map.clone();
                let written = written.clone();
                std::thread::spawn(move || {
                    let mut rounds = 0u32;
                    while let Some(seg) = map.acquire() {
                        let mut at = seg.pos();
                        loop {
                            let n = seg.writable(at, CHUNK);
                            if n == 0 {
                                break;
                            }
                            for cell in &written[at as usize..at as usize + n] {
                                cell.store(1, Ordering::Relaxed);
                            }
                            at += n as u64;
                            seg.commit(at);
                            rounds += 1;
                            // Uneven speeds make the steals interesting.
                            if rounds.is_multiple_of(worker + 2) {
                                std::thread::yield_now();
                            }
                        }
                        map.release(&seg);
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }

        assert!(map.is_complete());
        assert_eq!(map.downloaded(), TOTAL);
        assert!(is_partition(TOTAL, &map.snapshot()));
        assert!(written.iter().all(|c| c.load(Ordering::Relaxed) == 1), "a byte was never written");
    }
}
