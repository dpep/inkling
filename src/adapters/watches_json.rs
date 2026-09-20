//! Watches, their last known standing, and the event log — one JSON file.
//!
//! Deliberately not `state.json`, and deliberately not `waits.json` either.
//!
//! Against the snapshot: check state can go back to pending on a re-run, and
//! the snapshot's terminality means "stop polling forever" (docs/waiting.md).
//! Against the waits file: everything in there is disposable, rebuilt by the
//! next poll, which is why it has no quarantine and no version migration.
//! Nothing here is. A lost baseline re-emits or swallows an event, and a lost
//! cursor replays a subscriber's history or skips it. Opposite durability
//! contracts don't belong in one file.
//!
//! Every mutation goes through `update`, which is the load-modify-save
//! critical section under the lock. Polling is one call to `record` inside it,
//! so whatever process does the polling — a hook today, a shared short-lived
//! poller later — needs no state of its own and loses nothing when killed.

use crate::adapters::lock::FileLock;
use crate::domain::model::Reference;
use crate::domain::watch::{Event, Sighting, Standing, transitions};
use crate::ports::StoreError;
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const VERSION: u32 = 1;

/// How long a transition stays readable, and how long a silent subscriber
/// keeps its cursor.
///
/// Age alone bounds the log: pruning only what every subscriber has passed
/// would let one dead session pin it forever, and a transition nobody read in
/// a day is not something to act on — the work moved on without it.
pub const RETENTION: TimeDelta = TimeDelta::hours(24);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watches {
    pub version: u32,
    /// Keyed by the reference's display form, so watching the same thing
    /// twice is one watch rather than two.
    #[serde(default)]
    pub watches: BTreeMap<String, Watch>,
    /// Append-only within a retention window, ordered by `seq`.
    #[serde(default)]
    pub events: Vec<Event>,
    /// The highest sequence number handed out. The next event gets one more.
    #[serde(default)]
    pub last_seq: u64,
    /// Named cursors, for callers with nowhere to keep one.
    #[serde(default)]
    pub subscribers: BTreeMap<String, Subscriber>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watch {
    pub reference: Reference,
    pub since: DateTime<Utc>,
    /// Expiry is the cleanup, the same trick waits use: no reaper, no
    /// liveness check, and an abandoned watch costs a day of nothing.
    pub deadline: DateTime<Utc>,
    /// The baseline. `None` until the first successful reading, which is what
    /// makes that reading silent.
    #[serde(default)]
    pub seen: Option<Seen>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seen {
    pub at: DateTime<Utc>,
    #[serde(flatten)]
    pub standing: Standing,
    #[serde(default)]
    pub title: String,
}

/// What one pass of garbage collection dropped, so the log can say why the
/// file shrank rather than leaving it to be noticed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Pruned {
    pub watches: usize,
    pub events: usize,
    pub subscribers: usize,
}

impl Pruned {
    pub fn any(self) -> bool {
        self.watches + self.events + self.subscribers > 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscriber {
    pub cursor: u64,
    /// Last time this subscriber asked. A cursor nobody reads is dropped with
    /// the events it was pointing at.
    pub seen_at: DateTime<Utc>,
}

impl Watches {
    /// Start watching, or extend a watch already running.
    ///
    /// Re-watching keeps the baseline: resetting it would swallow every
    /// transition since the original subscription, which is exactly what the
    /// caller was asking not to miss.
    pub fn watch(&mut self, reference: &Reference, deadline: DateTime<Utc>, now: DateTime<Utc>) {
        let entry = self.watches.entry(reference.to_string());
        entry
            .and_modify(|watch| watch.deadline = deadline)
            .or_insert_with(|| Watch {
                reference: reference.clone(),
                since: now,
                deadline,
                seen: None,
            });
    }

    /// Stop watching. Reports whether there was anything to stop, so the CLI
    /// can say "not watching that" rather than claim a removal.
    pub fn unwatch(&mut self, reference: &Reference) -> bool {
        self.watches.remove(&reference.to_string()).is_some()
    }

    /// What is still worth asking about.
    pub fn active(&self, now: DateTime<Utc>) -> Vec<Reference> {
        self.watches
            .values()
            .filter(|watch| watch.deadline > now)
            .map(|watch| watch.reference.clone())
            .collect()
    }

    /// Whether the readings have aged past `refresh`, i.e. whether the next
    /// reader should pay for a poll before answering.
    ///
    /// A watch never read is due immediately; with no active watches nothing
    /// is due, which is what keeps a hook that watches nothing free.
    pub fn due(&self, now: DateTime<Utc>, refresh: TimeDelta) -> bool {
        self.watches
            .values()
            .filter(|watch| watch.deadline > now)
            .any(|watch| match &watch.seen {
                None => true,
                Some(seen) => now - seen.at >= refresh,
            })
    }

    /// Fold a poll's readings in, returning what changed.
    ///
    /// The one place events are created. A finished reference drops its watch
    /// after its last event is recorded — nothing follows merged, or Done.
    ///
    /// `observed_at` is when the readings were taken, not when they are being
    /// written. Two pollers overlapping would otherwise let the slower one's
    /// older reading land last and report a pull request moving backwards;
    /// stamping each reading with its own time makes that impossible rather
    /// than rare.
    pub fn record(
        &mut self,
        sightings: &[(Reference, Sighting)],
        observed_at: DateTime<Utc>,
    ) -> Vec<Event> {
        let mut fresh = Vec::new();
        for (reference, sighting) in sightings {
            let key = reference.to_string();
            let Some(watch) = self.watches.get_mut(&key) else {
                // Expired while the poll was in flight.
                continue;
            };
            if watch
                .seen
                .as_ref()
                .is_some_and(|seen| seen.at > observed_at)
            {
                continue;
            }
            // Logged where it happens, so `inkling logs` explains both an
            // event and the silence that precedes the first one. Titles and
            // summaries stay out: they're free text from elsewhere.
            if watch.seen.is_none() {
                tracing::info!(reference = %reference, "watch baselined");
            }
            for moved in transitions(watch.seen.as_ref().map(|s| &s.standing), sighting) {
                self.last_seq += 1;
                tracing::info!(
                    reference = %reference,
                    axis = moved.axis.as_str(),
                    from = moved.from,
                    to = moved.to,
                    seq = self.last_seq,
                    "transition"
                );
                fresh.push(Event {
                    seq: self.last_seq,
                    at: observed_at,
                    reference: reference.clone(),
                    axis: moved.axis,
                    from: moved.from,
                    to: moved.to,
                    title: sighting.title.clone(),
                    detail: moved.detail,
                });
            }
            watch.seen = Some(Seen {
                at: observed_at,
                standing: sighting.standing.clone(),
                title: sighting.title.clone(),
            });
            if sighting.standing.is_finished() {
                self.watches.remove(&key);
                tracing::info!(reference = %reference, "watch finished");
            }
        }
        self.events.extend(fresh.iter().cloned());
        fresh
    }

    /// Everything a named subscriber hasn't been told, advancing its cursor.
    ///
    /// A subscriber met for the first time starts at the head and hears
    /// nothing: the same rule as a watch's baseline, one level up. Without it
    /// a fresh session would be handed every transition another session had
    /// already acted on.
    ///
    /// The advance happens here rather than after the caller has printed,
    /// which makes this at-most-once: a process killed between the two loses
    /// that batch. The other order duplicates every batch whose reader died,
    /// forever, and duplicate news in a prompt is worse than missing news that
    /// the next poll will restate anyway.
    pub fn take(&mut self, subscriber: &str, now: DateTime<Utc>) -> Vec<Event> {
        let head = self.last_seq;
        let entry = self.subscribers.entry(subscriber.to_string());
        let cursor = entry
            .and_modify(|s| s.seen_at = now)
            .or_insert(Subscriber {
                cursor: head,
                seen_at: now,
            })
            .cursor;

        let due = crate::domain::watch::since(&self.events, cursor).to_vec();
        if let Some(entry) = self.subscribers.get_mut(subscriber) {
            entry.cursor = head;
        }
        tracing::debug!(
            subscriber,
            cursor_before = cursor,
            cursor_after = head,
            returned = due.len(),
            "events read"
        );
        due
    }

    /// Drop what has aged out: expired watches, events past retention, and
    /// the cursors of subscribers that stopped asking.
    pub fn collect_garbage(&mut self, now: DateTime<Utc>, retention: TimeDelta) -> Pruned {
        let before = (
            self.watches.len(),
            self.events.len(),
            self.subscribers.len(),
        );
        self.watches.retain(|_, watch| {
            let live = watch.deadline > now;
            if !live {
                tracing::info!(reference = %watch.reference, "watch expired");
            }
            live
        });
        self.events.retain(|event| now - event.at < retention);
        self.subscribers
            .retain(|_, subscriber| now - subscriber.seen_at < retention);
        Pruned {
            watches: before.0 - self.watches.len(),
            events: before.1 - self.events.len(),
            subscribers: before.2 - self.subscribers.len(),
        }
    }
}

/// `watches.json` and the lock around it.
pub struct WatchStore {
    path: PathBuf,
}

impl WatchStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// `$XDG_STATE_HOME/inkling/watches.json`, beside the run state.
    pub fn default_path() -> PathBuf {
        let mut path = crate::adapters::store_json::JsonStore::default_path();
        path.set_file_name("watches.json");
        path
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The lock a would-be poller tries for. Separate from the data lock
    /// because it is held across a network call: readers must not queue
    /// behind somebody else's GitHub request.
    pub fn poll_lock_path(&self) -> PathBuf {
        let mut name = self.path.file_name().unwrap_or_default().to_os_string();
        name.push(".poll.lock");
        self.path.with_file_name(name)
    }

    /// Read without taking the lock. Writes land by rename, so a reader sees
    /// one version or the other, never half of one.
    pub fn read(&self) -> Watches {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Watches>(&raw).ok())
            .filter(|watches| watches.version <= VERSION)
            .unwrap_or_default()
    }

    /// Load, tidy, change, save — all under the lock, which is the only way
    /// anything in here is written.
    pub fn update<T>(
        &self,
        now: DateTime<Utc>,
        change: impl FnOnce(&mut Watches) -> T,
    ) -> Result<T, StoreError> {
        let lock_path = FileLock::path_for(&self.path);
        let _lock = FileLock::acquire(&lock_path).map_err(|e| StoreError::Write(e.to_string()))?;

        let mut watches = self.load()?;
        let pruned = watches.collect_garbage(now, RETENTION);
        if pruned.any() {
            tracing::debug!(
                watches = pruned.watches,
                events = pruned.events,
                subscribers = pruned.subscribers,
                retention_hours = RETENTION.num_hours(),
                "pruned"
            );
        }
        let outcome = change(&mut watches);
        watches.version = VERSION;
        self.save(&watches)?;
        Ok(outcome)
    }

    fn load(&self) -> Result<Watches, StoreError> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Watches::default()),
            Err(e) => return Err(StoreError::Read(e.to_string())),
        };
        match serde_json::from_str::<Watches>(&raw) {
            Ok(watches) if watches.version > VERSION => Err(StoreError::FutureVersion {
                found: watches.version,
            }),
            Ok(watches) => Ok(watches),
            // Subscriptions and cursors can't be rebuilt from anywhere else,
            // so the unreadable file is kept for a look rather than replaced.
            Err(error) => {
                let moved = self.quarantine();
                tracing::warn!(%error, kept = ?moved, "watches file unreadable; starting over");
                Ok(Watches::default())
            }
        }
    }

    fn quarantine(&self) -> Option<PathBuf> {
        for n in 0..100 {
            let candidate = self.path.with_extension(format!("corrupt-{n}"));
            if !candidate.exists() {
                std::fs::rename(&self.path, &candidate).ok()?;
                return Some(candidate);
            }
        }
        None
    }

    fn save(&self, watches: &Watches) -> Result<(), StoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StoreError::Write(e.to_string()))?;
        }
        let json =
            serde_json::to_string_pretty(watches).map_err(|e| StoreError::Write(e.to_string()))?;
        let temp = self.path.with_extension("tmp");
        std::fs::write(&temp, json).map_err(|e| StoreError::Write(e.to_string()))?;
        std::fs::rename(&temp, &self.path).map_err(|e| StoreError::Write(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::State;
    use crate::domain::wait::CheckState;
    use crate::domain::watch::{Axis, Category};

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

    fn at(t: &str) -> DateTime<Utc> {
        t.parse().unwrap()
    }

    fn now() -> DateTime<Utc> {
        at("2026-09-20T12:00:00Z")
    }

    fn forever() -> DateTime<Utc> {
        at("2099-01-01T00:00:00Z")
    }

    fn seen_pull(state: State, checks: Option<CheckState>) -> Sighting {
        Sighting {
            standing: Standing::PullRequest { state, checks },
            title: "Add inkling wait".into(),
            failing: None,
        }
    }

    fn seen_issue(status: &str, category: Category) -> Sighting {
        Sighting {
            standing: Standing::Issue {
                status: status.into(),
                category,
            },
            title: "Try the watch feature".into(),
            failing: None,
        }
    }

    /// A store with one watch already baselined at `state`.
    fn watching(state: State) -> Watches {
        let mut watches = Watches::default();
        watches.watch(&pr(3), forever(), now());
        watches.record(&[(pr(3), seen_pull(state, None))], now());
        watches
    }

    #[test]
    fn the_first_reading_baselines_without_an_event() {
        let mut watches = Watches::default();
        watches.watch(&pr(3), forever(), now());

        let events = watches.record(&[(pr(3), seen_pull(State::Open, None))], now());
        assert!(events.is_empty(), "a baseline is not news");
        assert!(watches.events.is_empty());
        assert!(watches.watches["github.com/dpep/inkling#3"].seen.is_some());
    }

    #[test]
    fn the_second_reading_reports_what_moved() {
        let mut watches = watching(State::Open);
        let events = watches.record(&[(pr(3), seen_pull(State::Approved, None))], now());

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, 1, "sequence numbers start at one");
        assert_eq!(events[0].axis, Axis::State);
        assert_eq!(events[0].to, "approved");
        assert_eq!(events[0].title, "Add inkling wait");
    }

    #[test]
    fn a_finished_reference_stops_being_watched_after_its_last_event() {
        let mut watches = watching(State::Open);
        let events = watches.record(&[(pr(3), seen_pull(State::Merged, None))], now());

        assert_eq!(events.len(), 1, "the merge itself is still reported");
        assert!(
            watches.watches.is_empty(),
            "nothing follows merged, so nothing more is polled"
        );
    }

    #[test]
    fn a_red_suite_keeps_the_watch_alive() {
        // The reason watch terminality reads the lifecycle only: a re-run
        // takes red back to pending, and that is a third event worth having.
        let mut watches = watching(State::Open);
        watches.record(
            &[(pr(3), seen_pull(State::Open, Some(CheckState::Failure)))],
            now(),
        );
        assert_eq!(watches.watches.len(), 1);

        let events = watches.record(
            &[(pr(3), seen_pull(State::Open, Some(CheckState::Success)))],
            now(),
        );
        assert_eq!(events.len(), 1);
        assert_eq!(
            (events[0].from.as_str(), events[0].to.as_str()),
            ("failure", "success")
        );
    }

    #[test]
    fn re_watching_keeps_the_baseline_and_moves_the_deadline() {
        // Otherwise "make sure I'm still watching this" would quietly swallow
        // every transition since the original subscription.
        let mut watches = watching(State::Open);
        let later = at("2026-09-21T12:00:00Z");
        watches.watch(&pr(3), later, now());

        let watch = &watches.watches["github.com/dpep/inkling#3"];
        assert_eq!(watch.deadline, later);
        assert!(watch.seen.is_some(), "still baselined at open");

        let events = watches.record(&[(pr(3), seen_pull(State::Merged, None))], now());
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn an_expired_watch_stops_being_polled_and_is_collected() {
        let mut watches = Watches::default();
        watches.watch(&pr(3), at("2026-09-20T11:59:00Z"), now());
        assert!(watches.active(now()).is_empty(), "past its deadline");

        watches.collect_garbage(now(), RETENTION);
        assert!(watches.watches.is_empty());
    }

    #[test]
    fn a_reading_older_than_the_one_on_file_is_ignored() {
        // Two pollers overlapping: the slower one started first and lands
        // last. Without this its stale answer reports the pull request
        // moving backwards, and the subscriber acts on a retraction that
        // never happened.
        let mut watches = watching(State::Open);
        let later = now() + TimeDelta::seconds(30);
        watches.record(&[(pr(3), seen_pull(State::Merged, None))], later);

        let stale = watches.record(&[(pr(3), seen_pull(State::Open, None))], now());
        assert!(stale.is_empty(), "no merged → open");
    }

    #[test]
    fn a_reading_for_something_no_longer_watched_is_dropped() {
        let mut watches = Watches::default();
        let events = watches.record(&[(pr(3), seen_pull(State::Merged, None))], now());
        assert!(events.is_empty(), "nobody asked about it");
    }

    #[test]
    fn a_subscriber_is_told_each_transition_exactly_once() {
        // The contract the hook rests on: every prompt runs this, and a
        // transition must show up in exactly one of them.
        let mut watches = watching(State::Open);
        watches.take("session-a", now());
        watches.record(&[(pr(3), seen_pull(State::Approved, None))], now());

        let first = watches.take("session-a", now());
        assert_eq!(first.len(), 1, "delivered once");
        assert!(watches.take("session-a", now()).is_empty(), "and not again");
    }

    #[test]
    fn a_new_subscriber_starts_at_the_head_rather_than_the_beginning() {
        // A fresh session must not inherit transitions another session has
        // already acted on. Same rule as a watch's baseline, one level up.
        let mut watches = watching(State::Open);
        watches.record(&[(pr(3), seen_pull(State::Approved, None))], now());

        assert!(
            watches.take("session-b", now()).is_empty(),
            "history belongs to whoever was there for it"
        );
        watches.watch(&pr(3), forever(), now());
        watches.record(&[(pr(3), seen_pull(State::Merged, None))], now());
        assert_eq!(
            watches.take("session-b", now()).len(),
            1,
            "but the next one is its own"
        );
    }

    #[test]
    fn two_subscribers_each_get_their_own_copy() {
        let mut watches = watching(State::Open);
        watches.take("a", now());
        watches.take("b", now());
        watches.record(&[(pr(3), seen_pull(State::Approved, None))], now());

        assert_eq!(watches.take("a", now()).len(), 1);
        assert_eq!(
            watches.take("b", now()).len(),
            1,
            "a's read did not consume b's"
        );
    }

    #[test]
    fn an_unnamed_reader_asks_with_a_cursor_of_its_own() {
        let mut watches = watching(State::Open);
        watches.record(&[(pr(3), seen_pull(State::Approved, None))], now());

        let events = crate::domain::watch::since(&watches.events, 0);
        assert_eq!(events.len(), 1);
        assert!(crate::domain::watch::since(&watches.events, events[0].seq).is_empty());
    }

    #[test]
    fn nothing_is_due_when_nothing_is_watched() {
        // The hot path: a session watching nothing pays for no network.
        let watches = Watches::default();
        assert!(!watches.due(now(), TimeDelta::seconds(60)));
    }

    #[test]
    fn a_watch_is_due_until_it_has_been_read_and_again_once_the_reading_ages() {
        let mut watches = Watches::default();
        watches.watch(&pr(3), forever(), now());
        assert!(watches.due(now(), TimeDelta::seconds(60)), "never read");

        watches.record(&[(pr(3), seen_pull(State::Open, None))], now());
        assert!(!watches.due(now(), TimeDelta::seconds(60)), "just read");
        assert!(
            watches.due(now() + TimeDelta::seconds(60), TimeDelta::seconds(60)),
            "the reading aged out"
        );
    }

    #[test]
    fn an_expired_watch_does_not_keep_the_poller_busy() {
        let mut watches = Watches::default();
        watches.watch(
            &pr(3),
            at("2026-09-20T11:00:00Z"),
            at("2026-09-19T11:00:00Z"),
        );
        assert!(!watches.due(now(), TimeDelta::seconds(60)));
    }

    #[test]
    fn old_events_and_silent_subscribers_age_out_together() {
        let mut watches = watching(State::Open);
        watches.record(&[(pr(3), seen_pull(State::Approved, None))], now());
        watches.take("gone", now());

        let much_later = now() + TimeDelta::hours(25);
        watches.collect_garbage(much_later, RETENTION);

        assert!(
            watches.events.is_empty(),
            "a day-old transition is not news"
        );
        assert!(
            watches.subscribers.is_empty(),
            "a cursor nobody reads must not pin the log"
        );
    }

    #[test]
    fn an_issue_reports_its_status_moving() {
        let mut watches = Watches::default();
        watches.watch(&issue("AI-1"), forever(), now());
        watches.record(
            &[(issue("AI-1"), seen_issue("To Do", Category::New))],
            now(),
        );

        let events = watches.record(
            &[(
                issue("AI-1"),
                seen_issue("In Progress", Category::Indeterminate),
            )],
            now(),
        );
        assert_eq!(events[0].axis, Axis::Status);
        assert_eq!(events[0].reference, issue("AI-1"));
        assert_eq!(
            (events[0].from.as_str(), events[0].to.as_str()),
            ("To Do", "In Progress")
        );

        let done = watches.record(
            &[(issue("AI-1"), seen_issue("Done", Category::Done))],
            now(),
        );
        assert_eq!(done.len(), 1);
        assert!(watches.watches.is_empty(), "done is terminal");
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("inkling-watches-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("watches.json")
    }

    #[test]
    fn a_watch_and_its_cursor_survive_a_round_trip() {
        let store = WatchStore::new(scratch("round-trip"));
        store
            .update(now(), |watches| {
                watches.watch(&pr(3), forever(), now());
                watches.record(&[(pr(3), seen_pull(State::Open, None))], now());
                watches.record(&[(pr(3), seen_pull(State::Approved, None))], now());
            })
            .unwrap();

        let reloaded = store.read();
        assert_eq!(reloaded.version, VERSION);
        assert_eq!(reloaded.events.len(), 1);
        assert_eq!(
            reloaded.watches["github.com/dpep/inkling#3"]
                .seen
                .as_ref()
                .map(|s| &s.standing),
            Some(&Standing::PullRequest {
                state: State::Approved,
                checks: None
            })
        );
    }

    #[test]
    fn exactly_once_holds_across_processes_not_just_in_memory() {
        // Two separate reads of the same file, as two hook invocations would
        // do it. The cursor lives on disk or the guarantee is a fiction.
        let store = WatchStore::new(scratch("exactly-once"));
        store
            .update(now(), |watches| {
                watches.watch(&pr(3), forever(), now());
                watches.record(&[(pr(3), seen_pull(State::Open, None))], now());
                watches.take("hook", now());
                watches.record(&[(pr(3), seen_pull(State::Merged, None))], now());
            })
            .unwrap();

        let first = store.update(now(), |w| w.take("hook", now())).unwrap();
        assert_eq!(first.len(), 1);
        let second = store.update(now(), |w| w.take("hook", now())).unwrap();
        assert!(second.is_empty());
    }

    #[test]
    fn an_unreadable_file_is_kept_and_set_aside() {
        let path = scratch("corrupt");
        std::fs::write(&path, "{ not json").unwrap();
        let store = WatchStore::new(&path);

        store
            .update(now(), |w| w.watch(&pr(3), forever(), now()))
            .unwrap();

        assert_eq!(store.read().watches.len(), 1, "it carried on");
        assert!(
            path.with_extension("corrupt-0").exists(),
            "and kept the subscriptions it couldn't read"
        );
    }

    #[test]
    fn a_file_from_a_newer_version_is_refused_rather_than_overwritten() {
        let path = scratch("future");
        std::fs::write(&path, r#"{"version": 99}"#).unwrap();
        let store = WatchStore::new(&path);
        assert!(store.update(now(), |_| ()).is_err());
    }
}
