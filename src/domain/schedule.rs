//! When to look at a tracked message again.
//!
//! Checking every non-terminal record every run means a PR open for a week
//! costs ~500 GitHub lookups, nearly all of them worthless: the value of a
//! signal decays with the age of the Slack message, because nobody scrolls
//! back to a week-old post to see whether the PR landed.
//!
//! So the interval grows with the message's age rather than with a retry
//! count. Age is the thing that actually predicts whether anyone will read
//! the signal; a failure counter is only a proxy for it.

use crate::config::Tracking;
use chrono::{DateTime, TimeDelta, Utc};

/// How long to wait before re-checking a message of this age.
///
/// `base * 2^(age / doubling_period)`, capped. A zero base means no backoff
/// at all — every record, every run — which is a coherent thing to ask for
/// and needs no special case.
pub fn check_interval(tracking: &Tracking, age: TimeDelta) -> TimeDelta {
    let base = delta(tracking.base_check_interval);
    scale(base, doublings(tracking, age)).min(delta(tracking.max_check_interval))
}

/// When a message posted at `posted_at` and checked at `last_checked` is due
/// again.
pub fn next_check_at(
    tracking: &Tracking,
    posted_at: DateTime<Utc>,
    last_checked: DateTime<Utc>,
) -> DateTime<Utc> {
    last_checked + check_interval(tracking, last_checked - posted_at)
}

/// Past this, the message is old enough that nobody is coming back to it.
pub fn is_expired(tracking: &Tracking, posted_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    now - posted_at >= delta(tracking.max_tracking_age)
}

fn doublings(tracking: &Tracking, age: TimeDelta) -> u32 {
    let period = delta(tracking.doubling_period);
    if age <= TimeDelta::zero() || period <= TimeDelta::zero() {
        return 0;
    }
    // Saturating: an absurd age shouldn't overflow, and the result is capped
    // by max_check_interval anyway.
    u32::try_from(age.num_seconds() / period.num_seconds()).unwrap_or(u32::MAX)
}

fn scale(base: TimeDelta, doublings: u32) -> TimeDelta {
    // Saturating throughout: a very old message would otherwise overflow the
    // factor, and the result is clamped by max_check_interval regardless.
    let factor = 1_i64 << doublings.min(62);
    let secs = base.num_seconds().saturating_mul(factor);
    TimeDelta::try_seconds(secs).unwrap_or(TimeDelta::MAX)
}

fn delta(d: std::time::Duration) -> TimeDelta {
    TimeDelta::from_std(d).unwrap_or(TimeDelta::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tracking() -> Tracking {
        Tracking::default() // base 20m, doubling 1d, max 24h, horizon 7d
    }

    fn hours(n: i64) -> TimeDelta {
        TimeDelta::hours(n)
    }

    #[test]
    fn a_fresh_message_is_checked_at_the_base_interval() {
        assert_eq!(
            check_interval(&tracking(), hours(0)),
            TimeDelta::minutes(20)
        );
        assert_eq!(
            check_interval(&tracking(), hours(23)),
            TimeDelta::minutes(20)
        );
    }

    #[test]
    fn the_interval_doubles_with_each_day_of_age() {
        let t = tracking();
        assert_eq!(check_interval(&t, hours(24)), TimeDelta::minutes(40));
        assert_eq!(check_interval(&t, hours(48)), TimeDelta::minutes(80));
        assert_eq!(check_interval(&t, hours(72)), TimeDelta::minutes(160));
    }

    /// Over the 7-day horizon the curve never reaches the cap — the doubling
    /// is doing the work. The cap is a backstop for a longer horizon.
    #[test]
    fn the_default_curve_stays_under_the_cap_for_a_whole_horizon() {
        let t = tracking();
        let last_day = check_interval(&t, hours(24 * 6));
        assert_eq!(last_day, TimeDelta::minutes(20 * 64));
        assert!(last_day < delta(t.max_check_interval));
    }

    #[test]
    fn the_interval_stops_growing_at_the_cap() {
        let t = tracking();
        assert_eq!(
            check_interval(&t, hours(24 * 300)),
            delta(t.max_check_interval)
        );
    }

    #[test]
    fn a_zero_base_disables_backoff() {
        let t = Tracking {
            base_check_interval: Duration::ZERO,
            ..Tracking::default()
        };
        assert_eq!(check_interval(&t, hours(0)), TimeDelta::zero());
        assert_eq!(check_interval(&t, hours(24 * 5)), TimeDelta::zero());
    }

    #[test]
    fn due_time_is_the_last_check_plus_the_interval() {
        let t = tracking();
        let posted: DateTime<Utc> = "2026-08-01T00:00:00Z".parse().unwrap();
        let checked = posted + hours(48);
        assert_eq!(
            next_check_at(&t, posted, checked),
            checked + check_interval(&t, hours(48))
        );
    }

    #[test]
    fn a_message_expires_at_the_tracking_horizon() {
        let t = tracking();
        let posted: DateTime<Utc> = "2026-08-01T00:00:00Z".parse().unwrap();
        assert!(!is_expired(&t, posted, posted + hours(24 * 6)));
        assert!(is_expired(&t, posted, posted + hours(24 * 7)));
    }

    #[test]
    fn a_backdated_message_does_not_underflow() {
        // Clock skew can make a message look like it arrives from the future.
        assert_eq!(
            check_interval(&tracking(), hours(-5)),
            TimeDelta::minutes(20)
        );
    }
}
