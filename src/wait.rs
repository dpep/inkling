//! Blocking until something happens, on behalf of whoever is blocked.
//!
//! The judgement lives in `domain::wait`; this is the loop around it —
//! polling, sleeping, and deciding when to stop. Its one seam is `Observer`,
//! so a lone waiter asking GitHub directly and a group of waiters sharing one
//! poller (docs/waiting.md, W3) differ only in what is passed in.

use crate::domain::model::Reference;
use crate::domain::wait::{Observation, Resolution, Until, aggregate, resolve};
use crate::ports::{Clock, Observer, ProviderError, RefError, Sleeper};
use std::time::Duration;

/// A transient GitHub error shouldn't kill a thirty-minute wait, but a
/// persistent one shouldn't be waited out either.
const MAX_CONSECUTIVE_FAILURES: u32 = 5;

/// How a wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Met,
    /// Resolved against the condition — closed while waiting for a merge, or
    /// a red suite. A real answer, just not the hoped-for one.
    Precluded,
    TimedOut,
    /// No such pull request, or not visible with these credentials.
    Vanished,
}

impl Outcome {
    /// Distinct codes so a script can branch without parsing prose. 0 and 2
    /// keep the meaning they have in `run`; the rest are this command's.
    pub fn exit_code(self) -> i32 {
        match self {
            Outcome::Met => 0,
            Outcome::Precluded => 3,
            Outcome::TimedOut => 4,
            Outcome::Vanished => 5,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Met => "met",
            Outcome::Precluded => "precluded",
            Outcome::TimedOut => "timed_out",
            Outcome::Vanished => "vanished",
        }
    }
}

#[derive(Debug)]
pub struct Report {
    pub outcome: Outcome,
    pub until: Until,
    /// The last reading taken. Printed even on a timeout — it's the
    /// consolation prize the caller can act on.
    pub observations: Vec<Observation>,
    /// References that couldn't be read at all.
    pub vanished: Vec<Reference>,
    pub waited: Duration,
    pub polls: u32,
}

impl Report {
    pub fn exit_code(&self) -> i32 {
        self.outcome.exit_code()
    }
}

pub struct Wait<'a> {
    pub references: Vec<Reference>,
    pub until: Until,
    pub timeout: Duration,
    pub interval: Duration,
    pub observer: &'a dyn Observer,
    pub clock: &'a dyn Clock,
    pub sleeper: &'a dyn Sleeper,
}

impl Wait<'_> {
    pub fn execute(&self) -> Result<Report, ProviderError> {
        let started = self.clock.now();
        let mut polls = 0;
        let mut failures = 0;
        let mut last = Vec::new();
        // Where this wait started, set by the first complete reading.
        // `--until changed` measures against it; nothing else looks.
        let mut baseline = None;

        loop {
            polls += 1;
            let elapsed = || {
                (self.clock.now() - started)
                    .to_std()
                    .unwrap_or(Duration::ZERO)
            };

            match self.poll() {
                Ok(Poll::Vanished(gone)) => {
                    // Decisive whenever it happens, but the value is on the
                    // first poll: a typo shouldn't cost the full timeout.
                    return Ok(Report {
                        outcome: Outcome::Vanished,
                        until: self.until,
                        observations: Vec::new(),
                        vanished: gone,
                        waited: elapsed(),
                        polls,
                    });
                }
                // A shared poller answers with nothing until its first
                // reading lands. That is not "no references to judge" — the
                // fold is a minimum, so judging a subset could report met
                // while the missing reference is nowhere near.
                Ok(Poll::Incomplete) => {}
                Ok(Poll::Seen(observations)) => {
                    failures = 0;
                    last = observations;
                    // Before judging, not after: a pull request that is
                    // already finished should say so on the first poll
                    // rather than sit out an interval to discover it.
                    baseline = baseline.or_else(|| aggregate(&last));
                    let outcome = match resolve(self.until, &last, baseline) {
                        Resolution::Met => Some(Outcome::Met),
                        Resolution::Precluded => Some(Outcome::Precluded),
                        Resolution::Pending => None,
                    };
                    if let Some(outcome) = outcome {
                        return Ok(Report {
                            outcome,
                            until: self.until,
                            observations: last,
                            vanished: Vec::new(),
                            waited: elapsed(),
                            polls,
                        });
                    }
                }
                Err(error) => {
                    failures += 1;
                    if failures >= MAX_CONSECUTIVE_FAILURES {
                        return Err(error);
                    }
                    tracing::warn!(%error, attempt = failures, "poll failed; still waiting");
                }
            }

            // Checked after the poll, so a wait always takes at least one
            // reading even with a zero timeout — "is it merged yet" deserves
            // an answer, not an instant timeout.
            let spent = elapsed();
            let remaining = self.timeout.saturating_sub(spent);
            // Zero remaining ends it. Sleeping zero and looping would spin
            // forever against a clock that only moves when we sleep.
            if remaining.is_zero() {
                return Ok(Report {
                    outcome: Outcome::TimedOut,
                    until: self.until,
                    observations: last,
                    vanished: Vec::new(),
                    waited: spent,
                    polls,
                });
            }
            // Never sleep past the deadline: a wait's last nap is usually
            // shorter than the interval.
            self.sleeper.sleep(self.interval.min(remaining));
        }
    }

    fn poll(&self) -> Result<Poll, ProviderError> {
        let mut observations = Vec::new();
        let mut vanished = Vec::new();
        let mut failed: Option<String> = None;

        for (reference, status) in self.observer.observe(&self.references)? {
            match status {
                Ok(status) => observations.push(Observation {
                    reference,
                    state: status.state,
                    checks: status.checks,
                    title: status.title,
                }),
                Err(RefError::NotFound) => vanished.push(reference),
                // Rate limits and the like arrive here. Transient as far as
                // this loop is concerned; the retry budget decides.
                Err(RefError::Other(message)) => failed = Some(message),
            }
        }

        if !vanished.is_empty() {
            return Ok(Poll::Vanished(vanished));
        }
        if let Some(message) = failed {
            return Err(ProviderError::Transport(message));
        }
        match observations.len() < self.references.len() {
            true => Ok(Poll::Incomplete),
            false => Ok(Poll::Seen(observations)),
        }
    }
}

enum Poll {
    Seen(Vec<Observation>),
    Vanished(Vec<Reference>),
    /// Fewer readings than references — no verdict is safe on those.
    Incomplete,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::State;
    use crate::ports::{Lookup, RefStatus, StatusProvider};
    use crate::testing::FixedClock;
    use std::cell::RefCell;

    fn reference(number: u64) -> Reference {
        Reference::GitHubPullRequest {
            host: "github.com".into(),
            owner: "dpep".into(),
            repo: "inkling".into(),
            number,
        }
    }

    /// Hands back a scripted answer per poll, so a wait's whole arc is one
    /// list read top to bottom.
    struct Script {
        answers: RefCell<Vec<Result<Vec<Lookup>, ProviderError>>>,
        polls: RefCell<usize>,
    }

    impl Script {
        fn of(answers: Vec<Result<Vec<Lookup>, ProviderError>>) -> Self {
            Self {
                answers: RefCell::new(answers),
                polls: RefCell::new(0),
            }
        }

        /// One reference, one state, repeated forever.
        fn always(state: State) -> Self {
            Self::of(vec![Ok(vec![(reference(1), Ok(RefStatus::new(state)))])])
        }
    }

    impl StatusProvider for Script {
        fn lookup(&self, _references: &[Reference]) -> Result<Vec<Lookup>, ProviderError> {
            *self.polls.borrow_mut() += 1;
            let mut answers = self.answers.borrow_mut();
            // The last answer sticks, so "and then nothing changes" needs no
            // padding out to the poll count.
            match answers.len() {
                1 => answers[0].clone(),
                _ => answers.remove(0),
            }
        }
    }

    /// Sleeping moves the clock rather than the wall, so a thirty-minute
    /// wait is asserted in microseconds.
    struct Fast<'a>(&'a FixedClock);

    impl Sleeper for Fast<'_> {
        fn sleep(&self, duration: Duration) {
            self.0
                .advance(chrono::TimeDelta::from_std(duration).unwrap());
        }
    }

    fn wait_for(until: Until, script: &Script, timeout: Duration) -> Report {
        let clock = FixedClock::at("2026-08-25T12:00:00Z");
        let sleeper = Fast(&clock);
        Wait {
            references: vec![reference(1)],
            until,
            timeout,
            interval: Duration::from_secs(15),
            observer: script,
            clock: &clock,
            sleeper: &sleeper,
        }
        .execute()
        .expect("wait ran")
    }

    #[test]
    fn a_condition_already_true_returns_on_the_first_poll() {
        let report = wait_for(
            Until::Reached(State::Merged),
            &Script::always(State::Merged),
            Duration::from_secs(1800),
        );
        assert_eq!(report.outcome, Outcome::Met);
        assert_eq!(report.polls, 1, "no reason to sleep first");
        assert_eq!(report.waited, Duration::ZERO);
    }

    #[test]
    fn it_keeps_polling_until_the_state_arrives() {
        let script = Script::of(vec![
            Ok(vec![(reference(1), Ok(RefStatus::new(State::Open)))]),
            Ok(vec![(reference(1), Ok(RefStatus::new(State::Approved)))]),
            Ok(vec![(reference(1), Ok(RefStatus::new(State::Merged)))]),
        ]);
        let report = wait_for(
            Until::Reached(State::Merged),
            &script,
            Duration::from_secs(1800),
        );

        assert_eq!(report.outcome, Outcome::Met);
        assert_eq!(report.polls, 3);
        assert_eq!(report.waited, Duration::from_secs(30), "two naps of 15s");
    }

    #[test]
    fn a_wait_that_never_resolves_times_out_with_its_last_reading() {
        let report = wait_for(
            Until::Reached(State::Merged),
            &Script::always(State::Open),
            Duration::from_secs(60),
        );

        assert_eq!(report.outcome, Outcome::TimedOut);
        assert_eq!(report.exit_code(), 4);
        assert_eq!(
            report.observations.first().map(|o| o.state),
            Some(State::Open),
            "the last reading is the consolation prize"
        );
        assert!(report.waited <= Duration::from_secs(60), "never overshoots");
    }

    #[test]
    fn the_last_sleep_is_trimmed_to_the_deadline() {
        // A 20s timeout at a 15s interval must not sleep to 30s.
        let report = wait_for(
            Until::Reached(State::Merged),
            &Script::always(State::Open),
            Duration::from_secs(20),
        );
        assert_eq!(report.waited, Duration::from_secs(20));
        // Polls at 0s, 15s, and 20s: the deadline still gets a reading.
        assert_eq!(report.polls, 3);
    }

    #[test]
    fn closing_the_pull_request_ends_the_wait_early() {
        let script = Script::of(vec![
            Ok(vec![(reference(1), Ok(RefStatus::new(State::Open)))]),
            Ok(vec![(reference(1), Ok(RefStatus::new(State::Closed)))]),
        ]);
        let report = wait_for(
            Until::Reached(State::Merged),
            &script,
            Duration::from_secs(1800),
        );

        assert_eq!(report.outcome, Outcome::Precluded);
        assert_eq!(report.exit_code(), 3, "not a timeout — a real answer");
        assert_eq!(report.polls, 2);
    }

    #[test]
    fn an_unreadable_reference_fails_fast_rather_than_burning_the_timeout() {
        let script = Script::of(vec![Ok(vec![(reference(1), Err(RefError::NotFound))])]);
        let report = wait_for(
            Until::Reached(State::Merged),
            &script,
            Duration::from_secs(1800),
        );

        assert_eq!(report.outcome, Outcome::Vanished);
        assert_eq!(report.exit_code(), 5);
        assert_eq!(report.polls, 1);
        assert_eq!(report.vanished, vec![reference(1)]);
    }

    #[test]
    fn a_blip_does_not_end_a_long_wait() {
        let script = Script::of(vec![
            Err(ProviderError::Transport("HTTP 502".into())),
            Ok(vec![(reference(1), Ok(RefStatus::new(State::Merged)))]),
        ]);
        let report = wait_for(
            Until::Reached(State::Merged),
            &script,
            Duration::from_secs(1800),
        );

        assert_eq!(report.outcome, Outcome::Met);
        assert_eq!(report.polls, 2);
    }

    #[test]
    fn a_persistent_failure_gives_up_rather_than_waiting_it_out() {
        let script = Script::of(vec![Err(ProviderError::Transport("HTTP 401".into()))]);
        let clock = FixedClock::at("2026-08-25T12:00:00Z");
        let sleeper = Fast(&clock);
        let result = Wait {
            references: vec![reference(1)],
            until: Until::Reached(State::Merged),
            timeout: Duration::from_secs(1800),
            interval: Duration::from_secs(15),
            observer: &script,
            clock: &clock,
            sleeper: &sleeper,
        }
        .execute();

        assert!(result.is_err(), "a bad token is not something to sit out");
        assert_eq!(*script.polls.borrow(), MAX_CONSECUTIVE_FAILURES as usize);
    }

    #[test]
    fn a_wait_on_checks_ends_when_the_suite_settles() {
        use crate::domain::wait::CheckState;

        let pending = RefStatus {
            checks: Some(CheckState::Pending),
            ..RefStatus::new(State::Open)
        };
        let green = RefStatus {
            checks: Some(CheckState::Success),
            ..RefStatus::new(State::Open)
        };
        let script = Script::of(vec![
            Ok(vec![(reference(1), Ok(pending))]),
            Ok(vec![(reference(1), Ok(green))]),
        ]);
        let report = wait_for(Until::Checks, &script, Duration::from_secs(1800));

        assert_eq!(report.outcome, Outcome::Met);
        assert_eq!(report.polls, 2);
    }

    #[test]
    fn a_repository_without_checks_ends_the_wait_on_the_first_poll() {
        // RefStatus::new leaves checks None, which is what a repo running no
        // CI reports. Spinning for the full timeout would be the bug.
        let report = wait_for(
            Until::Checks,
            &Script::always(State::Open),
            Duration::from_secs(1800),
        );
        assert_eq!(report.outcome, Outcome::Precluded);
        assert_eq!(report.polls, 1);
    }

    #[test]
    fn a_partial_answer_is_not_judged() {
        // Waiting on two, told about one merged: folding over the subset
        // would report met while the other is untouched.
        let script = Script::of(vec![
            Ok(vec![(reference(1), Ok(RefStatus::new(State::Merged)))]),
            Ok(vec![
                (reference(1), Ok(RefStatus::new(State::Merged))),
                (reference(2), Ok(RefStatus::new(State::Open))),
            ]),
        ]);
        let clock = FixedClock::at("2026-08-25T12:00:00Z");
        let sleeper = Fast(&clock);
        let report = Wait {
            references: vec![reference(1), reference(2)],
            until: Until::Reached(State::Merged),
            timeout: Duration::from_secs(45),
            interval: Duration::from_secs(15),
            observer: &script,
            clock: &clock,
            sleeper: &sleeper,
        }
        .execute()
        .unwrap();

        assert_eq!(report.outcome, Outcome::TimedOut);
        assert_eq!(
            report.observations.len(),
            2,
            "the reading it did report is the complete one"
        );
    }

    #[test]
    fn every_outcome_has_its_own_exit_code() {
        // Scripts branch on these; two outcomes sharing a code would be a
        // silent behavior change for every caller.
        let codes: Vec<i32> = [
            Outcome::Met,
            Outcome::Precluded,
            Outcome::TimedOut,
            Outcome::Vanished,
        ]
        .iter()
        .map(|o| o.exit_code())
        .collect();
        assert_eq!(codes, vec![0, 3, 4, 5]);
    }
}
