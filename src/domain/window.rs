//! The discovery search window.
//!
//! Discovery and reconciliation are separate loops, and this only governs the
//! first: "have we searched every message posted since we last looked." A PR
//! that merges long after its message was posted is covered by the tracked
//! record, not by widening this window — which is what lets the window stay
//! small and clamped.

use crate::config::Window as WindowConfig;
use chrono::{DateTime, TimeDelta, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchWindow {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

impl SearchWindow {
    /// Empty windows happen when the clock jumps backward. Nothing to search,
    /// and nothing to advance.
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// Derive the next window from the last successful discovery.
///
/// The overlap re-scans a little before the last run so a message landing on
/// the boundary isn't missed; `max_lookback` clamps the whole thing so a
/// corrupt or ancient timestamp can't turn one run into an enormous query.
pub fn next_window(
    config: &WindowConfig,
    last_success: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> SearchWindow {
    let floor = now - delta(config.max_lookback);
    let start = match last_success {
        Some(last) => (last - delta(config.overlap)).max(floor),
        None => now - delta(config.initial_lookback),
    };
    SearchWindow { start, end: now }
}

fn delta(d: std::time::Duration) -> TimeDelta {
    TimeDelta::from_std(d).unwrap_or(TimeDelta::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn config() -> WindowConfig {
        WindowConfig::default() // overlap 10m, max_lookback 24h, initial 2h
    }

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn the_first_run_looks_back_by_the_initial_lookback() {
        let now = at("2026-08-12T12:00:00Z");
        let window = next_window(&config(), None, now);
        assert_eq!(window.start, at("2026-08-12T10:00:00Z"));
        assert_eq!(window.end, now);
    }

    #[test]
    fn a_normal_run_resumes_from_the_last_success_minus_the_overlap() {
        let now = at("2026-08-12T12:00:00Z");
        let last = at("2026-08-12T11:40:00Z");
        let window = next_window(&config(), Some(last), now);
        assert_eq!(window.start, at("2026-08-12T11:30:00Z"));
    }

    #[test]
    fn a_stale_timestamp_is_clamped_to_the_max_lookback() {
        // Laptop asleep for a month, or a corrupt state file: either way the
        // next run must not ask Slack for a month of history.
        let now = at("2026-08-12T12:00:00Z");
        let ancient = at("2026-01-01T00:00:00Z");
        let window = next_window(&config(), Some(ancient), now);
        assert_eq!(window.start, at("2026-08-11T12:00:00Z"));
    }

    #[test]
    fn a_missed_run_widens_the_window_up_to_the_clamp() {
        let now = at("2026-08-12T12:00:00Z");
        let last = at("2026-08-12T06:00:00Z");
        let window = next_window(&config(), Some(last), now);
        assert_eq!(window.start, at("2026-08-12T05:50:00Z"));
        assert!(!window.is_empty());
    }

    #[test]
    fn a_backward_clock_yields_an_empty_window() {
        let now = at("2026-08-12T12:00:00Z");
        let future = at("2026-08-12T18:00:00Z");
        let window = next_window(&config(), Some(future), now);
        assert!(window.is_empty(), "{window:?}");
    }

    #[test]
    fn a_zero_overlap_resumes_exactly_where_it_left_off() {
        let config = WindowConfig {
            overlap: Duration::ZERO,
            ..WindowConfig::default()
        };
        let now = at("2026-08-12T12:00:00Z");
        let last = at("2026-08-12T11:40:00Z");
        assert_eq!(next_window(&config, Some(last), now).start, last);
    }
}
