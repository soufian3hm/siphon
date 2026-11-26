//! The scheduler: when the queue of deferred downloads runs.
//!
//! Everything here is plain arithmetic on local wall-clock time so it can be
//! tested without waiting for a clock.

use serde::{Deserialize, Serialize};

pub const MINUTES_PER_DAY: i64 = 24 * 60;

/// What happens when the queue has nothing left to do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WhenDone {
    #[default]
    Nothing,
    /// Close the application.
    Quit,
    /// Put the computer to sleep.
    Sleep,
}

impl WhenDone {
    pub const ALL: [WhenDone; 3] = [WhenDone::Nothing, WhenDone::Quit, WhenDone::Sleep];

    pub fn label(self) -> &'static str {
        match self {
            WhenDone::Nothing => "Do nothing",
            WhenDone::Quit => "Quit Siphon",
            WhenDone::Sleep => "Sleep",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Schedule {
    /// Start the queue by itself at `start`.
    pub enabled: bool,
    /// Minutes after local midnight.
    pub start: u16,
    /// Stop the queue at `stop`, pausing what it started.
    pub stop_enabled: bool,
    pub stop: u16,
    /// Days the queue starts on, Monday first.
    pub days: [bool; 7],
    pub when_done: WhenDone,
}

impl Default for Schedule {
    fn default() -> Self {
        Self { enabled: false, start: 2 * 60, stop_enabled: false, stop: 7 * 60, days: [true; 7], when_done: WhenDone::Nothing }
    }
}

/// A point in local time, to the minute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalTime {
    /// Days since an arbitrary fixed origin; only differences matter.
    pub day: i64,
    /// Monday is zero.
    pub weekday: u8,
    /// Minutes after midnight.
    pub minute: u16,
}

impl LocalTime {
    /// The current time in the system's time zone.
    pub fn now() -> Self {
        let now = jiff::ZonedDateTime::now();
        // Seconds since the epoch, shifted into local time, name the local day.
        let local_seconds = now.timestamp().as_second() + i64::from(now.offset().seconds());
        Self {
            day: local_seconds.div_euclid(86_400),
            weekday: now.weekday().to_monday_zero_offset() as u8,
            minute: now.hour() as u16 * 60 + now.minute() as u16,
        }
    }

    fn absolute(&self) -> i64 {
        self.day * MINUTES_PER_DAY + i64::from(self.minute)
    }

    fn weekday_of(&self, day: i64) -> usize {
        (i64::from(self.weekday) + (day - self.day)).rem_euclid(7) as usize
    }
}

/// What the schedule asks for after time moved from one instant to another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Crossing {
    Start,
    Stop,
}

impl Schedule {
    pub fn sanitized(mut self) -> Self {
        self.start %= MINUTES_PER_DAY as u16;
        self.stop %= MINUTES_PER_DAY as u16;
        self
    }

    /// The scheduled moment, if any, that falls in `(prev, now]`.
    ///
    /// A long gap (the computer slept through the night) can contain both a
    /// start and a stop; the later of the two is the one that still applies.
    /// Gaps longer than a day are treated as one day: a schedule missed by
    /// more than that is not caught up on.
    pub fn crossing(&self, prev: LocalTime, now: LocalTime) -> Option<Crossing> {
        let to = now.absolute();
        let from = prev.absolute().clamp(to - MINUTES_PER_DAY, to);
        if from >= to {
            return None;
        }
        let latest = |minute: u16, respects_days: bool| -> Option<i64> {
            (now.day - 1..=now.day)
                .map(|day| (day, day * MINUTES_PER_DAY + i64::from(minute)))
                .filter(|(day, at)| *at > from && *at <= to && (!respects_days || self.days[now.weekday_of(*day)]))
                .map(|(_, at)| at)
                .max()
        };
        let start = if self.enabled { latest(self.start, true) } else { None };
        let stop = if self.stop_enabled { latest(self.stop, false) } else { None };
        match (start, stop) {
            (Some(start), Some(stop)) if stop >= start => Some(Crossing::Stop),
            (Some(_), _) => Some(Crossing::Start),
            (None, Some(_)) => Some(Crossing::Stop),
            (None, None) => None,
        }
    }

    /// Whether `now` lies inside today's (or last night's) running window.
    /// Only a schedule with both ends has a window.
    pub fn in_window(&self, now: LocalTime) -> bool {
        if !self.enabled || !self.stop_enabled || self.start == self.stop {
            return false;
        }
        let minute = now.minute;
        if self.start < self.stop {
            self.days[now.weekday as usize] && minute >= self.start && minute < self.stop
        } else {
            // Overnight: started yesterday evening, or starts this evening.
            let yesterday = (now.weekday as usize + 6) % 7;
            (self.days[now.weekday as usize] && minute >= self.start) || (self.days[yesterday] && minute < self.stop)
        }
    }

    /// Minutes from `now` until the next automatic start, if one will come.
    pub fn next_start_in(&self, now: LocalTime) -> Option<u32> {
        if !self.enabled {
            return None;
        }
        (0..=7i64).find_map(|ahead| {
            let day = now.day + ahead;
            let at = day * MINUTES_PER_DAY + i64::from(self.start);
            (self.days[now.weekday_of(day)] && at > now.absolute()).then(|| (at - now.absolute()) as u32)
        })
    }
}

/// `"02:30"` for 150.
pub fn format_minute(minute: u16) -> String {
    format!("{:02}:{:02}", minute / 60 % 24, minute % 60)
}

/// Parses `"2:30"`, `"02.30"`, `"0230"` or `"14"` into minutes after midnight.
pub fn parse_minute(text: &str) -> Option<u16> {
    let text = text.trim();
    let (hours, minutes) = match text.split_once([':', '.', 'h', ' ']) {
        Some((h, m)) => (h.trim(), m.trim().trim_end_matches('m')),
        None if text.len() > 2 && text.chars().all(|c| c.is_ascii_digit()) => text.split_at(text.len() - 2),
        None => (text, "0"),
    };
    let hours: u16 = hours.parse().ok()?;
    let minutes: u16 = if minutes.is_empty() { 0 } else { minutes.parse().ok()? };
    (hours < 24 && minutes < 60).then_some(hours * 60 + minutes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Day 0 is a Monday.
    fn at(day: i64, hour: u16, minute: u16) -> LocalTime {
        LocalTime { day, weekday: day.rem_euclid(7) as u8, minute: hour * 60 + minute }
    }

    fn nightly() -> Schedule {
        Schedule { enabled: true, start: 2 * 60, stop_enabled: true, stop: 6 * 60, ..Default::default() }
    }

    #[test]
    fn fires_once_when_the_minute_arrives() {
        let s = nightly();
        assert_eq!(s.crossing(at(0, 1, 58), at(0, 1, 59)), None);
        assert_eq!(s.crossing(at(0, 1, 59), at(0, 2, 0)), Some(Crossing::Start));
        assert_eq!(s.crossing(at(0, 2, 0), at(0, 2, 0)), None);
        assert_eq!(s.crossing(at(0, 2, 0), at(0, 2, 1)), None);
        assert_eq!(s.crossing(at(0, 5, 59), at(0, 6, 0)), Some(Crossing::Stop));
    }

    #[test]
    fn only_on_chosen_days() {
        let mut s = nightly();
        s.days = [true, false, true, false, true, false, false];
        assert_eq!(s.crossing(at(0, 1, 59), at(0, 2, 0)), Some(Crossing::Start));
        // Tuesday is off, but the stop time applies every day.
        assert_eq!(s.crossing(at(1, 1, 59), at(1, 2, 0)), None);
        assert_eq!(s.crossing(at(1, 5, 59), at(1, 6, 0)), Some(Crossing::Stop));
        assert_eq!(s.crossing(at(2, 1, 59), at(2, 2, 0)), Some(Crossing::Start));
    }

    #[test]
    fn waking_up_after_both_moments_keeps_only_the_last() {
        let s = nightly();
        // Asleep from 01:00 to 09:00: the window opened and closed unseen.
        assert_eq!(s.crossing(at(0, 1, 0), at(0, 9, 0)), Some(Crossing::Stop));
        // Asleep from 01:00 to 03:00: the window is open now.
        assert_eq!(s.crossing(at(0, 1, 0), at(0, 3, 0)), Some(Crossing::Start));
        // Across midnight.
        assert_eq!(s.crossing(at(0, 23, 0), at(1, 2, 30)), Some(Crossing::Start));
        // A week away counts as one day: only the last night is looked at,
        // and its window has closed again.
        assert_eq!(s.crossing(at(0, 12, 0), at(7, 12, 0)), Some(Crossing::Stop));
        assert_eq!(s.crossing(at(0, 12, 0), at(7, 1, 0)), Some(Crossing::Stop));
        assert_eq!(s.crossing(at(0, 12, 0), at(7, 3, 0)), Some(Crossing::Start));
    }

    #[test]
    fn a_clock_set_backwards_fires_nothing() {
        assert_eq!(nightly().crossing(at(0, 3, 0), at(0, 1, 0)), None);
    }

    #[test]
    fn disabled_parts_stay_quiet() {
        let s = Schedule { enabled: false, stop_enabled: true, ..nightly() };
        assert_eq!(s.crossing(at(0, 1, 59), at(0, 2, 0)), None);
        assert_eq!(s.crossing(at(0, 5, 59), at(0, 6, 0)), Some(Crossing::Stop));
        let s = Schedule { stop_enabled: false, ..nightly() };
        assert_eq!(s.crossing(at(0, 5, 59), at(0, 6, 0)), None);
        assert!(!s.in_window(at(0, 3, 0)));
    }

    #[test]
    fn windows_including_overnight_ones() {
        let s = nightly();
        assert!(s.in_window(at(0, 2, 0)));
        assert!(s.in_window(at(0, 5, 59)));
        assert!(!s.in_window(at(0, 6, 0)));
        assert!(!s.in_window(at(0, 1, 59)));

        let mut late = Schedule { enabled: true, start: 23 * 60, stop_enabled: true, stop: 5 * 60, ..Default::default() };
        late.days = [true, false, false, false, false, false, false];
        assert!(late.in_window(at(0, 23, 30)));
        // Tuesday morning still belongs to Monday night.
        assert!(late.in_window(at(1, 4, 0)));
        assert!(!late.in_window(at(1, 23, 30)));
        assert!(!late.in_window(at(0, 4, 0)));
    }

    #[test]
    fn next_start() {
        let mut s = nightly();
        assert_eq!(s.next_start_in(at(0, 1, 0)), Some(60));
        assert_eq!(s.next_start_in(at(0, 2, 0)), Some(24 * 60));
        s.days = [false, false, false, true, false, false, false];
        assert_eq!(s.next_start_in(at(0, 2, 0)), Some(3 * 24 * 60));
        s.days = [false; 7];
        assert_eq!(s.next_start_in(at(0, 2, 0)), None);
        s.enabled = false;
        assert_eq!(s.next_start_in(at(0, 1, 0)), None);
    }

    #[test]
    fn times_as_text() {
        assert_eq!(format_minute(150), "02:30");
        assert_eq!(format_minute(0), "00:00");
        assert_eq!(parse_minute("2:30"), Some(150));
        assert_eq!(parse_minute(" 02.30 "), Some(150));
        assert_eq!(parse_minute("0230"), Some(150));
        assert_eq!(parse_minute("14"), Some(14 * 60));
        assert_eq!(parse_minute("23:59"), Some(23 * 60 + 59));
        assert_eq!(parse_minute("24:00"), None);
        assert_eq!(parse_minute("7:75"), None);
        assert_eq!(parse_minute("soon"), None);
    }

    #[test]
    fn the_system_clock_is_readable() {
        let now = LocalTime::now();
        assert!(now.weekday < 7);
        assert!(now.minute < 1440);
        // 2026 is more than 20,000 days after the Unix epoch.
        assert!(now.day > 20_000);
    }
}
