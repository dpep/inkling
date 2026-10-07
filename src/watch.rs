//! Keeping watches current — the one step that turns readings into events.
//!
//! Everything that wants the feed fresh calls `poll`: today the `events`
//! command, when the readings have aged past its refresh interval; tomorrow
//! whatever else wants to, on whatever cadence it likes. There is no owner.
//! All the state is on disk and every change lands under the store's lock, so
//! a poller that dies mid-flight costs its own query and nothing else.
//!
//! The lock belongs to `poll_if_stale`, not to `poll`. Correctness is the
//! store's — see `poll` — so the lock saves a redundant request and nothing
//! more, which makes it the business of the caller that might be redundant.
//! No lease, no heartbeat, no takeover: unlike `waits.json`, the holder here
//! is not long-lived, so there is nothing to take over from.

use crate::adapters::lock::FileLock;
use crate::adapters::watches_json::WatchStore;
use crate::domain::model::Reference;
use crate::domain::watch::{Event, Sighting, Standing};
use crate::ports::{Clock, IssueProvider, StatusProvider, StoreError};
use chrono::TimeDelta;

pub struct Poller<'a> {
    pub store: &'a WatchStore,
    pub github: &'a dyn StatusProvider,
    pub jira: &'a dyn IssueProvider,
    pub clock: &'a dyn Clock,
}

/// What one call did, for the log line and for the caller's summary.
#[derive(Debug, Default)]
pub struct Polled {
    pub events: Vec<Event>,
    /// What was read, in full. The store keeps the live ones, but a reference
    /// that was already finished has its watch dropped on the way in, so this
    /// is the only place its standing survives — which is what lets `watch`
    /// say "already merged" rather than "watching nothing".
    pub sightings: Vec<(Reference, Sighting)>,
    /// References asked about.
    pub watched: usize,
    /// References that couldn't be read, with why. Not fatal: this runs on
    /// the prompt hot path, where an unreachable GitHub must degrade to
    /// silence rather than to a failed turn.
    pub failures: Vec<String>,
    /// Nothing was asked because the readings were still fresh, or because
    /// somebody else was already asking.
    pub skipped: bool,
}

/// How many unreadable references one poll names before it stops naming them.
///
/// A line per reference is what identifies the cause — wrong credentials, or a
/// watch that outlived what it points at — and the first few carry that as well
/// as all of them do. All of them is what a bad watch list costs: measured
/// 2026-09-20, a poll of 1,586 references wrote 838KiB of log, and a daemon
/// does that four times a minute. `polled watches` carries the count.
const NAMED_FAILURES: usize = 5;

fn name(so_far: &[String], reference: &Reference, error: &impl std::fmt::Display) {
    match so_far.len() < NAMED_FAILURES {
        true => tracing::warn!(%reference, %error, "could not read watch"),
        false => tracing::debug!(%reference, %error, "could not read watch"),
    }
}

impl Poller<'_> {
    /// Read every live watch that is due and record what moved. `floor` is
    /// the youngest reading this caller will pay to replace; the schedule in
    /// `watches_json::read_every` decides the rest.
    ///
    /// Takes no lock of its own. Correctness is the store's: `record` runs
    /// inside the data lock and compares against the baseline it finds there,
    /// so two pollers racing produce one set of events, not two. The poll
    /// lock below exists only to save a redundant request, which is why it
    /// belongs to the caller that might be redundant rather than to this.
    pub fn poll(&self, floor: TimeDelta, why: &str) -> Result<Polled, StoreError> {
        let observed_at = self.clock.now();
        let references = self.store.read().due_now(observed_at, floor);
        if references.is_empty() {
            tracing::debug!(why, "nothing is due");
            return Ok(Polled {
                skipped: true,
                ..Polled::default()
            });
        }

        let pulls = references
            .iter()
            .filter(|r| matches!(r, Reference::GitHubPullRequest { .. }))
            .count();
        let mut failures = Vec::new();
        let sightings = self.sightings(&references, &mut failures);
        let elapsed = (self.clock.now() - observed_at).num_milliseconds();

        let events = self.store.update(self.clock.now(), |watches| {
            watches.record(&sightings, observed_at)
        })?;

        tracing::info!(
            why,
            watched = references.len(),
            // Split by provider because they cost differently: pull requests
            // are one batched query, issues one `acli` apiece.
            github = pulls,
            jira = references.len() - pulls,
            read = sightings.len(),
            events = events.len(),
            failures = failures.len(),
            elapsed_ms = elapsed,
            "polled watches"
        );
        Ok(Polled {
            events,
            sightings,
            watched: references.len(),
            failures,
            skipped: false,
        })
    }

    /// Poll only if something is due, and only if nobody else is already
    /// doing it.
    ///
    /// This is the hook's path, run on every prompt, so the common case has
    /// to be the cheap one: a single small file read, no lock, no network.
    /// The lock is non-blocking — a caller that finds somebody mid-request
    /// has better things to do than queue for a result it can read a moment
    /// later.
    pub fn poll_if_stale(&self, floor: TimeDelta, why: &str) -> Result<Polled, StoreError> {
        if !self.store.read().due(self.clock.now(), floor) {
            tracing::debug!(why, "readings are fresh");
            return Ok(Polled {
                skipped: true,
                ..Polled::default()
            });
        }

        let lock_path = self.store.poll_lock_path();
        let held =
            FileLock::try_acquire(&lock_path).map_err(|e| StoreError::Write(e.to_string()))?;
        let Some(_lock) = held else {
            tracing::debug!(why, "another process is polling; reading what it writes");
            return Ok(Polled {
                skipped: true,
                ..Polled::default()
            });
        };
        self.poll(floor, why)
    }

    /// Ask each provider about the references it answers for.
    ///
    /// Routing by variant in one place, rather than a registry: with two
    /// providers a registry is indirection around a two-armed match, and the
    /// third one is when it starts paying for itself.
    fn sightings(
        &self,
        references: &[Reference],
        failures: &mut Vec<String>,
    ) -> Vec<(Reference, Sighting)> {
        let mut seen = Vec::new();

        let pulls: Vec<Reference> = references
            .iter()
            .filter(|r| matches!(r, Reference::GitHubPullRequest { .. }))
            .cloned()
            .collect();
        if !pulls.is_empty() {
            match self.github.lookup(&pulls) {
                Ok(lookups) => {
                    for (reference, status) in lookups {
                        match status {
                            Ok(status) => seen.push((
                                reference,
                                Sighting {
                                    standing: Standing::PullRequest {
                                        state: status.state,
                                        checks: status.checks,
                                    },
                                    title: status.title,
                                    failing: status.failing,
                                    head: status.head,
                                    approved_by: status.approved_by,
                                },
                            )),
                            Err(error) => {
                                name(failures, &reference, &error);
                                failures.push(format!("{reference}: {error}"));
                            }
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, references = pulls.len(), "github unreachable");
                    failures.push(error.to_string());
                }
            }
        }

        let keys: Vec<String> = references
            .iter()
            .filter_map(|r| match r {
                Reference::JiraIssue { key } => Some(key.clone()),
                _ => None,
            })
            .collect();
        if !keys.is_empty() {
            match self.jira.read(&keys) {
                Ok(lookups) => {
                    for (key, status) in lookups {
                        let reference = Reference::JiraIssue { key };
                        match status {
                            Ok(status) => seen.push((
                                reference,
                                Sighting {
                                    standing: Standing::Issue {
                                        status: status.name,
                                        category: status.category,
                                    },
                                    title: status.summary,
                                    failing: None,
                                    head: None,
                                    approved_by: Vec::new(),
                                },
                            )),
                            Err(error) => {
                                name(failures, &reference, &error);
                                failures.push(format!("{reference}: {error}"));
                            }
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, issues = keys.len(), "jira unreachable");
                    failures.push(error.to_string());
                }
            }
        }

        seen
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::watches_json::{FASTEST_READ, RETENTION};
    use crate::domain::model::State;
    use crate::domain::wait::CheckState;
    use crate::domain::watch::{Axis, Category};
    use crate::ports::{IssueLookup, IssueStatus, ProviderError, RefError};
    use crate::testing::{FixedClock, ScriptedStatusProvider};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::PathBuf;

    #[derive(Default)]
    struct ScriptedIssues {
        statuses: RefCell<HashMap<String, Result<IssueStatus, RefError>>>,
        unreachable: std::cell::Cell<bool>,
    }

    impl ScriptedIssues {
        fn set(&self, key: &str, name: &str, category: Category) {
            self.statuses.borrow_mut().insert(
                key.to_string(),
                Ok(IssueStatus {
                    name: name.into(),
                    category,
                    summary: "Try the watch feature".into(),
                }),
            );
        }
    }

    impl IssueProvider for ScriptedIssues {
        fn read(&self, keys: &[String]) -> Result<Vec<IssueLookup>, ProviderError> {
            if self.unreachable.get() {
                return Err(ProviderError::Transport("acli is not installed".into()));
            }
            let statuses = self.statuses.borrow();
            Ok(keys
                .iter()
                .map(|key| {
                    let status = statuses
                        .get(key)
                        .cloned()
                        .unwrap_or(Err(RefError::NotFound));
                    (key.clone(), status)
                })
                .collect())
        }
    }

    fn pr(number: u64) -> Reference {
        Reference::GitHubPullRequest {
            host: "github.com".into(),
            owner: "dpep".into(),
            repo: "inkling".into(),
            number,
        }
    }

    fn issue(key: &str) -> Reference {
        Reference::JiraIssue { key: key.into() }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("inkling-poll-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("watches.json")
    }

    /// Held for the life of a `Harness`.
    ///
    /// These tests take real `flock`s, and `flock` is per *file*, not per file
    /// descriptor — a dozen test threads in one process taking and releasing
    /// locks in the same directory tree is not the shape this code ever runs
    /// in, and it is not reliable. Real callers are separate processes taking
    /// one lock each, which is the case `a_concurrent_poller_is_left_to_it`
    /// covers. Serializing here tests the poll step rather than the harness.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct Harness {
        store: WatchStore,
        github: ScriptedStatusProvider,
        jira: ScriptedIssues,
        clock: FixedClock,
        _serial: std::sync::MutexGuard<'static, ()>,
    }

    impl Harness {
        fn new(name: &str) -> Self {
            Self {
                _serial: SERIAL.lock().unwrap_or_else(|e| e.into_inner()),
                store: WatchStore::new(scratch(name)),
                github: ScriptedStatusProvider::new(),
                jira: ScriptedIssues::default(),
                clock: FixedClock::at("2026-09-20T12:00:00Z"),
            }
        }

        fn poller(&self) -> Poller<'_> {
            Poller {
                store: &self.store,
                github: &self.github,
                jira: &self.jira,
                clock: &self.clock,
            }
        }

        /// Poll whatever is due, then let a reading's worth of time pass so
        /// the next call finds everything due again.
        fn poll(&self) -> Polled {
            let polled = self.poller().poll(TimeDelta::zero(), "test").unwrap();
            self.clock.advance(FASTEST_READ);
            polled
        }

        fn watch(&self, reference: &Reference) {
            let deadline = self.clock.now() + TimeDelta::hours(24);
            self.store
                .update(self.clock.now(), |w| {
                    w.watch(reference, "test", deadline, self.clock.now())
                })
                .unwrap();
        }
    }

    #[test]
    fn the_first_poll_baselines_and_the_next_one_reports() {
        let h = Harness::new("baseline");
        h.github.set(&pr(3), State::Open);
        h.watch(&pr(3));

        assert!(h.poll().events.is_empty());

        h.github.set(&pr(3), State::Approved);
        let polled = h.poll();
        assert_eq!(polled.events.len(), 1);
        assert_eq!(polled.events[0].axis, Axis::State);
        assert_eq!(polled.events[0].to, "approved");
    }

    #[test]
    fn both_kinds_of_reference_are_routed_in_one_pass() {
        let h = Harness::new("routing");
        h.github
            .set_checks(&pr(3), State::Open, CheckState::Pending);
        h.jira.set("AI-1", "To Do", Category::New);
        h.watch(&pr(3));
        h.watch(&issue("AI-1"));

        h.poll();
        h.github
            .set_checks(&pr(3), State::Open, CheckState::Failure);
        h.jira.set("AI-1", "In Progress", Category::Indeterminate);

        let polled = h.poll();
        assert_eq!(polled.watched, 2);
        let axes: Vec<_> = polled.events.iter().map(|e| e.axis).collect();
        assert!(axes.contains(&Axis::Checks), "{axes:?}");
        assert!(axes.contains(&Axis::Status), "{axes:?}");
    }

    #[test]
    fn fresh_readings_cost_no_request() {
        // The hot path, and the reason a hook can run this on every prompt.
        let h = Harness::new("fresh");
        h.github.set(&pr(3), State::Open);
        h.watch(&pr(3));
        h.poller().poll(TimeDelta::zero(), "test").unwrap();
        let after_first = h.github.calls();
        let stale = |h: &Harness| {
            !h.poller()
                .poll_if_stale(TimeDelta::seconds(60), "test")
                .unwrap()
                .skipped
        };

        h.clock.advance(TimeDelta::seconds(59));
        assert!(!stale(&h));
        assert_eq!(h.github.calls(), after_first, "no second request");

        h.clock.advance(TimeDelta::seconds(1));
        assert!(stale(&h));
        assert_eq!(h.github.calls(), after_first + 1);
    }

    #[test]
    fn a_quiet_watch_is_left_alone_between_its_readings() {
        // A day of nothing: the prompt path's 60s floor no longer means every
        // prompt a minute apart pays for a request.
        let h = Harness::new("quiet");
        h.github.set(&pr(3), State::Open);
        h.watch(&pr(3));
        h.clock.advance(TimeDelta::hours(24));
        h.poller().poll(TimeDelta::zero(), "test").unwrap();
        let calls = h.github.calls();

        h.clock.advance(TimeDelta::minutes(59));
        let polled = h
            .poller()
            .poll_if_stale(TimeDelta::seconds(60), "test")
            .unwrap();
        assert!(polled.skipped);
        assert_eq!(h.github.calls(), calls);
    }

    #[test]
    fn watching_nothing_asks_nothing() {
        let h = Harness::new("empty");
        assert!(h.poll().skipped);
        assert_eq!(h.github.calls(), 0);
    }

    #[test]
    fn a_concurrent_poller_is_left_to_it() {
        // Two hooks firing on the same prompt must not both query.
        let h = Harness::new("contended");
        h.github.set(&pr(3), State::Open);
        h.watch(&pr(3));

        // Held for the rest of the test. Taking it again after releasing it
        // would be a second lock on one file from one process, which is not
        // what this ever does: a real caller takes it once and exits. That a
        // free lock is taken is what every other test here relies on.
        let _held = FileLock::try_acquire(&h.store.poll_lock_path())
            .unwrap()
            .expect("free");

        let polled = h
            .poller()
            .poll_if_stale(TimeDelta::seconds(60), "test")
            .unwrap();
        assert!(polled.skipped);
        assert_eq!(
            h.github.calls(),
            0,
            "the second caller reads what the first writes rather than asking too"
        );
    }

    #[test]
    fn an_unreadable_reference_is_reported_without_stopping_the_others() {
        let h = Harness::new("partial");
        h.github.set(&pr(3), State::Open);
        h.jira.set("AI-1", "To Do", Category::New);
        h.watch(&pr(3));
        h.watch(&issue("AI-2")); // never set, so not found
        h.watch(&issue("AI-1"));

        let polled = h.poll();
        assert_eq!(polled.failures.len(), 1, "{:?}", polled.failures);
        assert!(polled.failures[0].contains("AI-2"));

        let stored = h.store.read();
        assert!(stored.watches["github.com/dpep/inkling#3"].seen.is_some());
        assert!(
            stored.watches["AI-1"].seen.is_some(),
            "its sibling still baselined"
        );
    }

    #[test]
    fn an_unreachable_provider_is_silence_not_a_failure() {
        // A hook on the prompt path must degrade to saying nothing rather
        // than to a failed turn.
        let h = Harness::new("offline");
        h.watch(&issue("AI-1"));
        h.jira.unreachable.set(true);

        let polled = h.poll();
        assert!(polled.events.is_empty());
        assert_eq!(polled.failures.len(), 1);
        assert!(!polled.skipped, "it tried");
    }

    #[test]
    fn a_finished_reference_is_polled_once_more_and_then_never() {
        let h = Harness::new("finished");
        h.github.set(&pr(3), State::Open);
        h.watch(&pr(3));
        h.poll();

        h.github.set(&pr(3), State::Merged);
        assert_eq!(h.poll().events.len(), 1);

        let calls = h.github.calls();
        assert!(h.poll().skipped, "nothing left to watch");
        assert_eq!(h.github.calls(), calls);
    }

    #[test]
    fn readings_and_cursors_outlive_the_process_that_took_them() {
        // The property a shared poller will rest on: nothing is held in
        // memory, so whoever polls next carries on from what is on disk.
        let h = Harness::new("durable");
        h.github.set(&pr(3), State::Open);
        h.watch(&pr(3));
        h.poll();
        h.github.set(&pr(3), State::Approved);
        h.poll();

        let reread = WatchStore::new(h.store.path());
        let events = reread
            .update(h.clock.now(), |w| w.take("hook", h.clock.now()))
            .unwrap();
        assert!(events.is_empty(), "a new subscriber starts at the head");

        let mut fresh = reread.read();
        assert_eq!(fresh.events.len(), 1);
        assert_eq!(fresh.collect_garbage(h.clock.now(), RETENTION).events, 0);
    }

    #[test]
    fn a_reference_the_provider_ignores_does_not_look_like_a_failure() {
        // GitHub answers about pull requests only; asking it about a Jira key
        // returns nothing rather than an error, and the Jira provider covers
        // it in the same pass.
        let h = Harness::new("ignored");
        h.jira.set("AI-1", "To Do", Category::New);
        h.watch(&issue("AI-1"));

        let polled = h.poll();
        assert!(polled.failures.is_empty(), "{:?}", polled.failures);
        assert_eq!(h.github.calls(), 0, "no pull requests, no github request");
    }

    #[test]
    fn a_reading_carries_the_check_that_failed_into_the_event() {
        let h = Harness::new("failing");
        h.github
            .set_checks(&pr(3), State::Open, CheckState::Pending);
        h.watch(&pr(3));
        h.poll();

        h.github.fail_check(&pr(3), State::Open, "test / build");
        let polled = h.poll();
        assert_eq!(polled.events[0].detail.as_deref(), Some("test / build"));
    }

    #[test]
    fn a_provider_error_does_not_disturb_the_baseline() {
        // Otherwise a blip would re-baseline and swallow the transition that
        // happened while GitHub was unreachable.
        let h = Harness::new("blip");
        h.github.set(&pr(3), State::Open);
        h.watch(&pr(3));
        h.poll();

        h.github.fail(&pr(3), RefError::Other("HTTP 502".into()));
        h.poll();

        h.github.set(&pr(3), State::Merged);
        let polled = h.poll();
        assert_eq!(polled.events.len(), 1, "open → merged, not swallowed");
        assert_eq!(polled.events[0].from, "open");
    }

    #[test]
    fn nothing_is_recorded_for_a_reference_whose_status_never_arrived() {
        let h = Harness::new("absent");
        h.watch(&pr(3));
        h.github.fail(&pr(3), RefError::NotFound);

        h.poll();
        assert!(
            h.store.read().watches["github.com/dpep/inkling#3"]
                .seen
                .is_none(),
            "a reference that couldn't be read has no baseline to record"
        );
    }
}
