//! Speed measurement and the global speed limit.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::time::Instant as Clock;

/// Sliding-window throughput meter.
///
/// Fed with a monotonically increasing byte counter a few times per second.
/// The reported speed is the slope across the window, which is steady enough
/// to read and still reacts to a change within a couple of seconds.
#[derive(Debug)]
pub struct SpeedMeter {
    window: Duration,
    samples: VecDeque<(Instant, u64)>,
    history: VecDeque<f32>,
    history_cap: usize,
    last_history: Option<Instant>,
    history_step: Duration,
}

impl Default for SpeedMeter {
    fn default() -> Self {
        Self::new(Duration::from_millis(2500), 96, Duration::from_millis(500))
    }
}

impl SpeedMeter {
    pub fn new(window: Duration, history_cap: usize, history_step: Duration) -> Self {
        Self {
            window,
            samples: VecDeque::with_capacity(16),
            history: VecDeque::with_capacity(history_cap),
            history_cap,
            last_history: None,
            history_step,
        }
    }

    /// Records the counter value at `now`.
    pub fn sample(&mut self, now: Instant, counter: u64) {
        self.samples.push_back((now, counter));
        // Keep one sample older than the window so the slope spans all of it.
        while self.samples.len() > 2 && now.duration_since(self.samples[1].0) >= self.window {
            self.samples.pop_front();
        }
        if self.last_history.is_none_or(|t| now.duration_since(t) >= self.history_step) {
            self.last_history = Some(now);
            if self.history.len() == self.history_cap {
                self.history.pop_front();
            }
            self.history.push_back(self.bytes_per_sec() as f32);
        }
    }

    /// Current speed in bytes per second.
    pub fn bytes_per_sec(&self) -> f64 {
        let (Some(&(t0, c0)), Some(&(t1, c1))) = (self.samples.front(), self.samples.back()) else {
            return 0.0;
        };
        let dt = t1.duration_since(t0).as_secs_f64();
        if dt < 0.05 { 0.0 } else { c1.saturating_sub(c0) as f64 / dt }
    }

    /// Recent speeds, oldest first, for the sparkline.
    pub fn history(&self) -> Vec<f32> {
        self.history.iter().copied().collect()
    }

    /// Forgets everything; the next samples start a new measurement.
    pub fn reset(&mut self) {
        self.samples.clear();
    }
}

/// Seconds left at the current speed, or `None` when it cannot be estimated.
pub fn eta_secs(remaining: u64, bytes_per_sec: f64) -> Option<u64> {
    if remaining == 0 {
        return Some(0);
    }
    // Below ~1 KiB/s the estimate is noise.
    (bytes_per_sec >= 1024.0).then(|| (remaining as f64 / bytes_per_sec).ceil() as u64)
}

/// Global download speed limit shared by every worker of every download.
///
/// A virtual-clock pacer: each caller reserves the time its bytes take at the
/// configured rate and sleeps until its reservation begins. That is exact
/// over time, fair between callers, and costs one uncontended lock per chunk.
#[derive(Debug)]
pub struct Limiter {
    /// Bytes per second; zero means unlimited.
    rate: AtomicU64,
    next_free: Mutex<Clock>,
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new(0)
    }
}

impl Limiter {
    /// How far a caller may run ahead of the clock, so that short bursts are
    /// not punished and tiny chunks do not each cost a timer.
    const BURST: Duration = Duration::from_millis(150);

    pub fn new(bytes_per_sec: u64) -> Self {
        Self { rate: AtomicU64::new(bytes_per_sec), next_free: Mutex::new(Clock::now()) }
    }

    pub fn set_rate(&self, bytes_per_sec: u64) {
        self.rate.store(bytes_per_sec, Ordering::Relaxed);
    }

    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    /// Waits until `bytes` may pass.
    pub async fn take(&self, bytes: usize) {
        let rate = self.rate.load(Ordering::Relaxed);
        if rate == 0 || bytes == 0 {
            return;
        }
        let cost = Duration::from_secs_f64(bytes as f64 / rate as f64);
        let wake = {
            let mut next_free = self.next_free.lock();
            let now = Clock::now();
            // An idle limiter does not bank unlimited credit.
            let start = (*next_free).max(now.checked_sub(Self::BURST).unwrap_or(now));
            *next_free = start + cost;
            start
        };
        tokio::time::sleep_until(wake).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_reports_the_slope_over_the_window() {
        let mut m = SpeedMeter::new(Duration::from_secs(2), 8, Duration::from_millis(500));
        let t0 = Instant::now();
        assert_eq!(m.bytes_per_sec(), 0.0);
        for i in 0..=20u64 {
            m.sample(t0 + Duration::from_millis(i * 250), i * 250_000);
        }
        let speed = m.bytes_per_sec();
        assert!((speed - 1_000_000.0).abs() < 1.0, "{speed}");

        // The transfer stalls: the speed decays to zero within one window.
        for i in 21..=30u64 {
            m.sample(t0 + Duration::from_millis(i * 250), 20 * 250_000);
        }
        assert_eq!(m.bytes_per_sec(), 0.0);
        assert!(m.history().len() <= 8);
    }

    #[test]
    fn eta() {
        assert_eq!(eta_secs(0, 0.0), Some(0));
        assert_eq!(eta_secs(10_000_000, 1_000_000.0), Some(10));
        assert_eq!(eta_secs(10_000_000, 100.0), None);
    }

    #[tokio::test(start_paused = true)]
    async fn limiter_paces_to_the_configured_rate() {
        let limiter = Limiter::new(1_000_000);
        let start = tokio::time::Instant::now();
        for _ in 0..40 {
            limiter.take(100_000).await;
        }
        // 4 MB at 1 MB/s, minus the burst allowance.
        let elapsed = start.elapsed().as_secs_f64();
        assert!((3.6..=4.1).contains(&elapsed), "{elapsed}");
    }

    #[tokio::test(start_paused = true)]
    async fn unlimited_never_waits() {
        let limiter = Limiter::new(0);
        let start = tokio::time::Instant::now();
        limiter.take(usize::MAX).await;
        assert_eq!(start.elapsed(), Duration::ZERO);
    }
}
