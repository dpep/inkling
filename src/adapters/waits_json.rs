//! One poll serving every waiter, coordinated through a file.
//!
//! Waiters are ordinary short-lived processes, not clients of a daemon. Each
//! registers what it is watching in `waits.json`; one of them at a time holds
//! a *lease* and does the polling for everybody, writing the readings back.
//! The rest read those readings locally and judge their own condition. The
//! existing provider already folds N references into one aliased query, so
//! "poll once for everyone" is that call with a longer list — the dedup is
//! not new machinery, it's the batching that was already there.
//!
//! Crashed waiters need no reaper. Every watch carries the deadline its
//! `--timeout` implies, and whoever next holds the file drops the expired
//! ones, so the orphan-record problem is defined away rather than handled.
//!
//! Deliberately not `state.json`: the cron run never reads this file and
//! waiters never write that one, so waiting adds no contention to the
//! product that exists. The cost is two sources of truth about a pull
//! request, accepted because they want different freshness — the reaction
//! loop backs off as a message ages, while a wait is the opposite fact,
//! someone blocked right now.

use crate::adapters::lock::FileLock;
use crate::domain::model::{Reference, State};
use crate::domain::wait::CheckState;
use crate::ports::{Clock, Lookup, Observer, ProviderError, RefError, RefStatus, StatusProvider};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waits {
    pub version: u32,
    /// Who is polling on everyone's behalf, if anyone.
    #[serde(default)]
    pub lease: Option<Lease>,
    /// Active waits, keyed by an id unique to the waiting process.
    #[serde(default)]
    pub watches: BTreeMap<String, Watch>,
    /// Last reading per reference, keyed by its display form.
    #[serde(default)]
    pub readings: BTreeMap<String, Reading>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub holder: String,
    /// Refreshed on every successful poll. A lease that stops being
    /// refreshed is taken over — which is what makes a killed holder
    /// harmless rather than a stall.
    pub heartbeat: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watch {
    pub references: Vec<Reference>,
    /// When this waiter gives up anyway. Doubles as its expiry here.
    pub deadline: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reading {
    pub at: DateTime<Utc>,
    #[serde(flatten)]
    pub outcome: Seen,
}

/// What the last poll made of one reference. Mirrors `Lookup`'s in-band
/// failure: one unreadable repository must not spoil the file for the rest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Seen {
    Ok {
        state: State,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checks: Option<CheckState>,
        #[serde(default)]
        title: String,
    },
    NotFound,
    Failed {
        message: String,
    },
}

impl Waits {
    /// `$XDG_STATE_HOME/inkling/waits.json`, beside the run state.
    pub fn default_path() -> PathBuf {
        let mut path = crate::adapters::store_json::JsonStore::default_path();
        path.set_file_name("waits.json");
        path
    }

    /// Drop what nobody is waiting on any more.
    ///
    /// Expired watches go first, then readings no surviving watch mentions —
    /// otherwise the file grows once per pull request ever waited on.
    fn collect_garbage(&mut self, now: DateTime<Utc>) {
        self.watches.retain(|_, watch| watch.deadline > now);

        let wanted: std::collections::BTreeSet<String> = self
            .watches
            .values()
            .flat_map(|watch| watch.references.iter().map(ToString::to_string))
            .collect();
        self.readings
            .retain(|reference, _| wanted.contains(reference));
    }

    /// Every reference anyone is waiting on, deduplicated. This list is the
    /// whole point: one query covers it.
    fn all_references(&self) -> Vec<Reference> {
        let mut references: Vec<Reference> = self
            .watches
            .values()
            .flat_map(|watch| watch.references.iter().cloned())
            .collect();
        references.sort();
        references.dedup();
        references
    }

    /// Take the lease if it's free or its holder has gone quiet.
    ///
    /// Reports *how* it went, not just whether: a takeover means a waiter
    /// died or stalled, which is worth counting rather than inferring from a
    /// gap in the logs.
    fn claim(&mut self, holder: &str, now: DateTime<Utc>, stale_after: Duration) -> Claim {
        let claim = match &self.lease {
            None => Claim::Free,
            Some(lease) if lease.holder == holder => Claim::Held,
            Some(lease) => {
                let quiet = (now - lease.heartbeat).to_std().unwrap_or(Duration::ZERO);
                match quiet > stale_after {
                    true => Claim::TakenOver,
                    false => Claim::Denied,
                }
            }
        };
        if claim.polls() {
            self.lease = Some(Lease {
                holder: holder.to_string(),
                heartbeat: now,
            });
        }
        claim
    }

    /// The readings for one waiter's references — but only as a complete set.
    ///
    /// A partial answer would be worse than none: the condition folds with
    /// `min`, so judging two pull requests on one reading could report met
    /// when the missing one is nowhere near.
    fn complete_reading(
        &self,
        references: &[Reference],
        now: DateTime<Utc>,
        fresh_for: Duration,
    ) -> Option<Vec<Lookup>> {
        references
            .iter()
            .map(|reference| {
                let reading = self.readings.get(&reference.to_string())?;
                let age = (now - reading.at).to_std().unwrap_or(Duration::ZERO);
                if age > fresh_for {
                    return None;
                }
                let status = match &reading.outcome {
                    Seen::Ok {
                        state,
                        checks,
                        title,
                    } => Ok(RefStatus {
                        state: *state,
                        author: String::new(),
                        title: title.clone(),
                        checks: *checks,
                        // Not persisted: a wait reports that checks went red,
                        // not which job did.
                        failing: None,
                    }),
                    Seen::NotFound => Err(RefError::NotFound),
                    Seen::Failed { message } => Err(RefError::Other(message.clone())),
                };
                Some((reference.clone(), status))
            })
            .collect()
    }

    fn record(&mut self, lookups: &[Lookup], now: DateTime<Utc>) {
        for (reference, status) in lookups {
            let outcome = match status {
                Ok(status) => Seen::Ok {
                    state: status.state,
                    checks: status.checks,
                    title: status.title.clone(),
                },
                Err(RefError::NotFound) => Seen::NotFound,
                Err(RefError::Other(message)) => Seen::Failed {
                    message: message.clone(),
                },
            };
            self.readings
                .insert(reference.to_string(), Reading { at: now, outcome });
        }
    }
}

/// How this process came by the lease on one tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Claim {
    /// Nobody held it.
    Free,
    /// We already did.
    Held,
    /// The holder stopped reporting, so we stepped in.
    TakenOver,
    /// Somebody else is polling; read their readings instead.
    Denied,
}

impl Claim {
    fn polls(self) -> bool {
        !matches!(self, Claim::Denied)
    }
}

/// What actually happened while waiting — enough to answer whether the
/// coordination earns itself, which docs/waiting.md names as the open
/// question this design should be judged on.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Ticks where this process queried GitHub, for itself and everyone else.
    pub polled: u32,
    /// Ticks where another waiter's reading answered instead — the saving.
    pub shared: u32,
    /// Ticks with no complete fresh reading to read. Non-zero means waiters
    /// are asking faster than readings arrive, which argues for a shorter
    /// interval rather than for more polling.
    pub starved: u32,
    /// Most watches in the file at once, this one included. Stuck at 1 means
    /// the lease is coordination without load.
    pub peak_watches: usize,
    /// Most references covered by one of our polls. Above the number we
    /// asked for is the dedup doing work.
    pub peak_batch: usize,
    /// Times we found a holder that had gone quiet and stepped in.
    pub takeovers: u32,
}

/// Reads and writes `waits.json` under the lock, and polls when it holds the
/// lease. One per waiting process.
pub struct SharedObserver<'a> {
    path: PathBuf,
    /// Unique to this process — pid alone is recycled.
    holder: String,
    deadline: DateTime<Utc>,
    /// How stale a reading may be before it stops counting, and how quiet a
    /// lease holder may go before it's taken over. Both are the poll interval
    /// with slack, since that's the rate readings arrive at.
    tolerance: Duration,
    provider: &'a dyn StatusProvider,
    clock: &'a dyn Clock,
    stats: std::cell::RefCell<Stats>,
}

impl<'a> SharedObserver<'a> {
    pub fn new(
        path: impl Into<PathBuf>,
        deadline: DateTime<Utc>,
        interval: Duration,
        provider: &'a dyn StatusProvider,
        clock: &'a dyn Clock,
    ) -> Self {
        // pid is recycled, and a clock can read the same twice, so neither
        // alone identifies a waiter. The counter settles the rest.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let started = clock.now();
        Self {
            path: path.into(),
            holder: format!(
                "{}-{}-{seq}",
                std::process::id(),
                started.timestamp_millis()
            ),
            deadline,
            tolerance: interval.saturating_mul(2),
            provider,
            clock,
            stats: std::cell::RefCell::default(),
        }
    }

    /// What this waiter saw. Read once, at the end, for the summary line.
    pub fn stats(&self) -> Stats {
        *self.stats.borrow()
    }

    fn lock_path(&self) -> PathBuf {
        FileLock::path_for(&self.path)
    }

    /// A missing or unreadable file is an empty one. Nothing here is
    /// authoritative — every entry is rebuilt by the next poll — so quarantine
    /// would be ceremony.
    fn load(&self) -> Waits {
        let parsed = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Waits>(&raw).ok())
            .filter(|waits| waits.version <= VERSION);
        parsed.unwrap_or(Waits {
            version: VERSION,
            ..Waits::default()
        })
    }

    fn save(&self, waits: &Waits) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(waits)?;
        let temp = self.path.with_extension("tmp");
        std::fs::write(&temp, json)?;
        std::fs::rename(&temp, &self.path)
    }

    /// Stop other waiters polling on our behalf once we're done.
    pub fn deregister(&self) {
        let Ok(_lock) = FileLock::acquire(&self.lock_path()) else {
            return;
        };
        let mut waits = self.load();
        waits.watches.remove(&self.holder);
        // Hand the lease on rather than making the next waiter wait out the
        // staleness window for a process that has already exited.
        if waits
            .lease
            .as_ref()
            .is_some_and(|l| l.holder == self.holder)
        {
            waits.lease = None;
        }
        waits.collect_garbage(self.clock.now());
        let _ = self.save(&waits);
    }
}

impl Observer for SharedObserver<'_> {
    fn observe(&self, references: &[Reference]) -> Result<Vec<Lookup>, ProviderError> {
        let now = self.clock.now();

        // First pass under the lock: register, tidy, and find out whether
        // this process is the one that polls this tick.
        let (polling, existing) = {
            let _lock = FileLock::acquire(&self.lock_path())
                .map_err(|e| ProviderError::Transport(e.to_string()))?;
            let mut waits = self.load();
            waits.collect_garbage(now);
            waits.watches.insert(
                self.holder.clone(),
                Watch {
                    references: references.to_vec(),
                    deadline: self.deadline,
                },
            );
            let claim = waits.claim(&self.holder, now, self.tolerance);
            let existing = waits.complete_reading(references, now, self.tolerance);
            let wanted = waits.all_references();

            let mut stats = self.stats.borrow_mut();
            stats.peak_watches = stats.peak_watches.max(waits.watches.len());
            if claim == Claim::TakenOver {
                stats.takeovers += 1;
                tracing::debug!(
                    watches = waits.watches.len(),
                    "took over the poller lease from a waiter that went quiet"
                );
            }
            drop(stats);

            self.save(&waits)
                .map_err(|e| ProviderError::Transport(e.to_string()))?;
            (claim.polls().then_some(wanted), existing)
        };

        let Some(wanted) = polling else {
            // Somebody else is polling. Their readings, or nothing yet —
            // nothing yet reads as "keep waiting", which is correct.
            let mut stats = self.stats.borrow_mut();
            stats.shared += 1;
            if existing.is_none() {
                stats.starved += 1;
            }
            return Ok(existing.unwrap_or_default());
        };

        {
            let mut stats = self.stats.borrow_mut();
            stats.polled += 1;
            stats.peak_batch = stats.peak_batch.max(wanted.len());
        }

        // The lock is not held across this: it's a network call, and holding
        // it would block every other waiter for its duration.
        let lookups = self.provider.lookup(&wanted)?;
        let polled_at = self.clock.now();

        let _lock = FileLock::acquire(&self.lock_path())
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        let mut waits = self.load();
        waits.record(&lookups, polled_at);
        // Only a successful poll refreshes the heartbeat, so a holder that
        // can't reach GitHub loses the lease to someone who maybe can.
        waits.lease = Some(Lease {
            holder: self.holder.clone(),
            heartbeat: polled_at,
        });
        self.save(&waits)
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        Ok(waits
            .complete_reading(references, polled_at, self.tolerance)
            .unwrap_or_default())
    }
}

impl Drop for SharedObserver<'_> {
    fn drop(&mut self) {
        self.deregister();
    }
}

/// Read the file for display. Cheap, and takes no lock — a torn read isn't
/// possible, since writes land by rename.
pub fn read(path: &Path) -> Option<Waits> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FixedClock, ScriptedStatusProvider};

    fn reference(number: u64) -> Reference {
        Reference::GitHubPullRequest {
            host: "github.com".into(),
            owner: "dpep".into(),
            repo: "inkling".into(),
            number,
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("inkling-waits-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("waits.json")
    }

    fn at(t: &str) -> DateTime<Utc> {
        t.parse().unwrap()
    }

    /// A watch that never expires during a test.
    fn watch(references: Vec<Reference>) -> Watch {
        Watch {
            references,
            deadline: at("2099-01-01T00:00:00Z"),
        }
    }

    #[test]
    fn one_poll_covers_every_waiters_references() {
        // The dedup, stated: three waiters, overlapping interests, one list.
        let mut waits = Waits::default();
        waits
            .watches
            .insert("a".into(), watch(vec![reference(1), reference(2)]));
        waits
            .watches
            .insert("b".into(), watch(vec![reference(2), reference(3)]));
        waits.watches.insert("c".into(), watch(vec![reference(1)]));

        assert_eq!(
            waits.all_references(),
            vec![reference(1), reference(2), reference(3)],
            "each reference asked for exactly once"
        );
    }

    #[test]
    fn an_expired_watch_and_its_readings_are_collected() {
        let now = at("2026-08-25T12:00:00Z");
        let mut waits = Waits::default();
        waits.watches.insert(
            "gone".into(),
            Watch {
                references: vec![reference(1)],
                deadline: at("2026-08-25T11:59:00Z"),
            },
        );
        waits
            .watches
            .insert("live".into(), watch(vec![reference(2)]));
        waits.record(
            &[
                (reference(1), Ok(RefStatus::new(State::Open))),
                (reference(2), Ok(RefStatus::new(State::Open))),
            ],
            now,
        );

        waits.collect_garbage(now);

        assert_eq!(waits.watches.keys().collect::<Vec<_>>(), vec!["live"]);
        assert_eq!(
            waits.readings.keys().collect::<Vec<_>>(),
            vec![&reference(2).to_string()],
            "a reading nobody waits on is dropped, or the file grows forever"
        );
    }

    #[test]
    fn a_deadline_is_all_the_cleanup_a_dead_waiter_needs() {
        // No reaper, no pid liveness check: the timeout every wait must have
        // anyway is what removes it.
        let mut waits = Waits::default();
        waits.watches.insert(
            "crashed".into(),
            Watch {
                references: vec![reference(1)],
                deadline: at("2026-08-25T12:00:00Z"),
            },
        );
        waits.collect_garbage(at("2026-08-25T12:00:01Z"));
        assert!(waits.watches.is_empty());
    }

    #[test]
    fn a_live_lease_holder_keeps_everyone_else_out() {
        let now = at("2026-08-25T12:00:00Z");
        let mut waits = Waits::default();
        assert!(waits.claim("first", now, Duration::from_secs(30)).polls());
        assert!(
            !waits.claim("second", now, Duration::from_secs(30)).polls(),
            "one poller at a time is the whole point"
        );
    }

    #[test]
    fn a_quiet_lease_is_taken_over() {
        let mut waits = Waits::default();
        waits.claim("first", at("2026-08-25T12:00:00Z"), Duration::from_secs(30));

        assert!(
            waits
                .claim(
                    "second",
                    at("2026-08-25T12:00:31Z"),
                    Duration::from_secs(30)
                )
                .polls(),
            "a holder that stopped reporting must not stall every waiter"
        );
        assert_eq!(waits.lease.unwrap().holder, "second");
    }

    #[test]
    fn the_holder_keeps_its_own_lease() {
        let mut waits = Waits::default();
        let now = at("2026-08-25T12:00:00Z");
        waits.claim("me", now, Duration::from_secs(30));
        assert!(waits.claim("me", now, Duration::from_secs(30)).polls());
    }

    #[test]
    fn a_partial_reading_is_no_reading() {
        // Waiting on two, holding one: folding with min over the one present
        // could report met while the other is untouched.
        let now = at("2026-08-25T12:00:00Z");
        let mut waits = Waits::default();
        waits.record(&[(reference(1), Ok(RefStatus::new(State::Merged)))], now);

        assert!(
            waits
                .complete_reading(&[reference(1), reference(2)], now, Duration::from_secs(30))
                .is_none()
        );
        assert!(
            waits
                .complete_reading(&[reference(1)], now, Duration::from_secs(30))
                .is_some()
        );
    }

    #[test]
    fn a_stale_reading_does_not_count() {
        let mut waits = Waits::default();
        waits.record(
            &[(reference(1), Ok(RefStatus::new(State::Merged)))],
            at("2026-08-25T12:00:00Z"),
        );
        assert!(
            waits
                .complete_reading(
                    &[reference(1)],
                    at("2026-08-25T12:00:31Z"),
                    Duration::from_secs(30)
                )
                .is_none(),
            "an old answer would resolve a wait on stale news"
        );
    }

    #[test]
    fn readings_survive_a_round_trip_through_the_file() {
        let clock = FixedClock::at("2026-08-25T12:00:00Z");
        let provider = ScriptedStatusProvider::new();
        provider.set(&reference(1), State::Approved);
        let path = scratch("round-trip");

        let observer = SharedObserver::new(
            &path,
            at("2099-01-01T00:00:00Z"),
            Duration::from_secs(15),
            &provider,
            &clock,
        );
        let lookups = observer.observe(&[reference(1)]).unwrap();
        assert_eq!(
            lookups[0].1.as_ref().map(|s| s.state),
            Ok(State::Approved),
            "the poller reads its own writing back"
        );

        let on_disk = read(&path).expect("the file was written");
        assert_eq!(on_disk.version, VERSION);
        assert_eq!(on_disk.lease.unwrap().holder, observer.holder);
    }

    #[test]
    fn a_second_waiter_reads_rather_than_polls() {
        let clock = FixedClock::at("2026-08-25T12:00:00Z");
        let provider = ScriptedStatusProvider::new();
        provider.set(&reference(1), State::Open);
        let path = scratch("shared");
        let deadline = at("2099-01-01T00:00:00Z");

        let first =
            SharedObserver::new(&path, deadline, Duration::from_secs(15), &provider, &clock);
        first.observe(&[reference(1)]).unwrap();
        let after_first = provider.calls();

        let second =
            SharedObserver::new(&path, deadline, Duration::from_secs(15), &provider, &clock);
        let lookups = second.observe(&[reference(1)]).unwrap();

        assert_eq!(
            provider.calls(),
            after_first,
            "the second waiter cost no GitHub call — this is the dedup"
        );
        assert_eq!(lookups[0].1.as_ref().map(|s| s.state), Ok(State::Open));
    }

    #[test]
    fn the_poller_covers_references_it_is_not_itself_waiting_on() {
        let clock = FixedClock::at("2026-08-25T12:00:00Z");
        let provider = ScriptedStatusProvider::new();
        provider.set(&reference(1), State::Open);
        provider.set(&reference(2), State::Merged);
        let path = scratch("coverage");
        let deadline = at("2099-01-01T00:00:00Z");

        // Registers interest in #2, then finds someone else already polling.
        let bystander =
            SharedObserver::new(&path, deadline, Duration::from_secs(15), &provider, &clock);
        bystander.observe(&[reference(2)]).unwrap();

        let holder =
            SharedObserver::new(&path, deadline, Duration::from_secs(15), &provider, &clock);
        holder.observe(&[reference(1)]).unwrap();

        let on_disk = read(&path).unwrap();
        assert!(
            on_disk.readings.contains_key(&reference(2).to_string()),
            "the lease holder polls for everyone, not just itself"
        );
    }

    #[test]
    fn deregistering_frees_the_lease_for_the_next_waiter() {
        let clock = FixedClock::at("2026-08-25T12:00:00Z");
        let provider = ScriptedStatusProvider::new();
        provider.set(&reference(1), State::Open);
        let path = scratch("handover");

        {
            let observer = SharedObserver::new(
                &path,
                at("2099-01-01T00:00:00Z"),
                Duration::from_secs(15),
                &provider,
                &clock,
            );
            observer.observe(&[reference(1)]).unwrap();
        }

        let on_disk = read(&path).unwrap();
        assert!(
            on_disk.lease.is_none(),
            "a finished waiter hands the lease on rather than leaving it to time out"
        );
        assert!(on_disk.watches.is_empty());
    }

    #[test]
    fn the_stats_show_who_polled_and_who_read() {
        // The measurement the open question turns on: a second waiter must
        // show up as `shared`, not as another `polled`.
        let clock = FixedClock::at("2026-08-25T12:00:00Z");
        let provider = ScriptedStatusProvider::new();
        provider.set(&reference(1), State::Open);
        let path = scratch("stats");
        let deadline = at("2099-01-01T00:00:00Z");

        let holder =
            SharedObserver::new(&path, deadline, Duration::from_secs(15), &provider, &clock);
        holder.observe(&[reference(1)]).unwrap();
        let reader =
            SharedObserver::new(&path, deadline, Duration::from_secs(15), &provider, &clock);
        reader.observe(&[reference(1)]).unwrap();

        assert_eq!(holder.stats().polled, 1);
        assert_eq!(holder.stats().shared, 0);
        assert_eq!(
            reader.stats().polled,
            0,
            "the second waiter queried nothing"
        );
        assert_eq!(reader.stats().shared, 1);
        assert_eq!(
            reader.stats().peak_watches,
            2,
            "fan-out is what makes the sharing worth having"
        );
    }

    #[test]
    fn a_poll_covering_more_than_we_asked_for_is_visible_in_the_stats() {
        // peak_batch above `watching` is the dedup doing work; equal to it
        // means the lease is coordination without load.
        let clock = FixedClock::at("2026-08-25T12:00:00Z");
        let provider = ScriptedStatusProvider::new();
        provider.set(&reference(1), State::Open);
        provider.set(&reference(2), State::Open);
        let path = scratch("batch");
        let deadline = at("2099-01-01T00:00:00Z");

        // The first observer takes the free lease, so it is the poller; the
        // second only registers. The poller's *next* tick is the one that
        // covers both.
        let holder =
            SharedObserver::new(&path, deadline, Duration::from_secs(15), &provider, &clock);
        holder.observe(&[reference(1)]).unwrap();
        let bystander =
            SharedObserver::new(&path, deadline, Duration::from_secs(15), &provider, &clock);
        bystander.observe(&[reference(2)]).unwrap();
        holder.observe(&[reference(1)]).unwrap();

        assert_eq!(
            holder.stats().peak_batch,
            2,
            "one reference of its own, two covered"
        );
    }

    #[test]
    fn a_takeover_is_counted() {
        let clock = FixedClock::at("2026-08-25T12:00:00Z");
        let provider = ScriptedStatusProvider::new();
        provider.set(&reference(1), State::Open);
        let path = scratch("takeover-stats");
        let deadline = at("2099-01-01T00:00:00Z");

        let dead = SharedObserver::new(&path, deadline, Duration::from_secs(15), &provider, &clock);
        dead.observe(&[reference(1)]).unwrap();
        std::mem::forget(dead); // a killed waiter never deregisters

        clock.advance(chrono::TimeDelta::seconds(31));
        let next = SharedObserver::new(&path, deadline, Duration::from_secs(15), &provider, &clock);
        next.observe(&[reference(1)]).unwrap();

        assert_eq!(next.stats().takeovers, 1);
    }

    #[test]
    fn an_unreadable_file_starts_over_rather_than_failing() {
        let path = scratch("corrupt");
        std::fs::write(&path, "{ this is not json").unwrap();
        let clock = FixedClock::at("2026-08-25T12:00:00Z");
        let provider = ScriptedStatusProvider::new();
        provider.set(&reference(1), State::Open);

        let observer = SharedObserver::new(
            &path,
            at("2099-01-01T00:00:00Z"),
            Duration::from_secs(15),
            &provider,
            &clock,
        );
        assert!(observer.observe(&[reference(1)]).is_ok());
    }

    #[test]
    fn a_reading_carries_its_check_state() {
        let now = at("2026-08-25T12:00:00Z");
        let mut waits = Waits::default();
        waits.record(
            &[(
                reference(1),
                Ok(RefStatus {
                    checks: Some(CheckState::Failure),
                    ..RefStatus::new(State::Open)
                }),
            )],
            now,
        );
        let lookups = waits
            .complete_reading(&[reference(1)], now, Duration::from_secs(30))
            .unwrap();
        assert_eq!(
            lookups[0].1.as_ref().unwrap().checks,
            Some(CheckState::Failure)
        );
    }

    #[test]
    fn an_unreadable_reference_survives_the_round_trip() {
        // A shared file must not turn one bad reference into everyone's
        // problem, nor lose the fact that it was bad.
        let now = at("2026-08-25T12:00:00Z");
        let mut waits = Waits::default();
        waits.record(&[(reference(1), Err(RefError::NotFound))], now);
        let lookups = waits
            .complete_reading(&[reference(1)], now, Duration::from_secs(30))
            .unwrap();
        assert_eq!(lookups[0].1, Err(RefError::NotFound));
    }
}
