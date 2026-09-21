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
use crate::adapters::session_socket::SessionAddress;
use crate::domain::model::Reference;
use crate::domain::watch::{Event, Sighting, Standing, reading};
use crate::ports::StoreError;
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const VERSION: u32 = 1;

/// The file holds session tokens, so nobody else on the machine reads it.
const OWNER_ONLY: u32 = 0o600;

/// How long a transition stays readable.
///
/// Age alone bounds the log: pruning only what every subscriber has passed
/// would let one dead session pin it forever. Three days rather than one so a
/// session resumed after a weekend still reads what moved on Friday; the log
/// is small enough that the extra two days cost nothing worth measuring.
pub const RETENTION: TimeDelta = TimeDelta::hours(72);

/// How long a watch lasts unless `--ttl` says otherwise.
///
/// The same as `RETENTION`, so what a watch reported is readable for as long
/// as the watch could have been reporting it — a longer watch would outlive
/// its own early news.
pub const WATCH_TTL: TimeDelta = RETENTION;

/// How often a watch is read: 1/`QUIET_SHARE` of how long it has been quiet,
/// clamped to [`FASTEST_READ`, `SLOWEST_READ`].
///
/// A placeholder, and deliberately one pure function so retuning it is one
/// edit — docs/deferred.md says what would change it. Quiet under 24 minutes
/// reads every minute, six hours every quarter hour, a day or more hourly: a
/// 72h watch that never moves costs ~170 reads rather than 4,320.
pub const QUIET_SHARE: i32 = 24;
pub const FASTEST_READ: TimeDelta = TimeDelta::minutes(1);
pub const SLOWEST_READ: TimeDelta = TimeDelta::minutes(60);

pub fn read_every(quiet: TimeDelta) -> TimeDelta {
    (quiet / QUIET_SHARE).clamp(FASTEST_READ, SLOWEST_READ)
}

/// How long a subscriber record outlives its last sign of life.
///
/// Deliberately longer than the event log, and counted in days rather than
/// hours: a subscribed session that is waiting for something is silent by
/// definition — that is the whole case for pushing to it — so an hour of
/// quiet says nothing about whether it is still there. Dropping the record
/// would drop the address push needs, and the cursor a resumed session
/// returns to.
pub const SUBSCRIBER_RETENTION: TimeDelta = TimeDelta::days(7);

/// How many messages one session may be sent in `PUSH_WINDOW`.
///
/// A push costs the session a turn, so this is a ceiling on what inkling can
/// cost one session however badly the thing it is watching behaves. Measured
/// 2026-09-20: a pull request whose review decision oscillated produced one
/// push per tick — seven in 13 seconds — and nothing stood between that and
/// the session until the watch's deadline. Five is more than any
/// five minutes of genuine news, and what it withholds stays in the feed for
/// the next prompt.
pub const PUSH_BUDGET: usize = 5;
pub const PUSH_WINDOW: TimeDelta = TimeDelta::minutes(5);

/// How long a writer waits for the lock before giving up.
///
/// The critical section is a small file read, a tidy and a rename — measured
/// at 2ms. This is a hundred times that, and still short enough that nobody
/// watching a session notice it.
const PATIENCE: std::time::Duration = std::time::Duration::from_millis(250);

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
    /// Which readers asked for this. Never empty in practice: every watch is
    /// registered by somebody, a session or the terminal.
    ///
    /// Two sessions watching the same pull request is one watch and one poll,
    /// and its transitions reach both.
    #[serde(default)]
    pub owners: BTreeSet<String>,
    /// The last time a session touched this or it moved — what its reading
    /// schedule counts quiet from. Absent in files that predate the schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stirred_at: Option<DateTime<Utc>>,
}

impl Watch {
    pub fn quiet_since(&self) -> DateTime<Utc> {
        self.stirred_at.unwrap_or(self.since)
    }

    /// When this is next worth reading, for a caller that won't pay for a
    /// reading younger than `floor`. Never read means due since registration.
    ///
    /// Measured from the last reading rather than from now, so the answer is
    /// a fixed instant a server can sleep until.
    pub fn next_read(&self, floor: TimeDelta) -> DateTime<Utc> {
        match &self.seen {
            None => self.since,
            Some(seen) => seen.at + read_every(seen.at - self.quiet_since()).max(floor),
        }
    }
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

/// What unwatching found, so the CLI can say it rather than guess.
///
/// Three answers because there are three truths: an owner withdrawing from
/// something another session also watches is neither a removal nor a no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unwatched {
    Absent,
    /// This owner stopped caring; another session still does, so one poll
    /// still serves it.
    StillWatched,
    Removed,
}

/// A named cursor, which may also carry somewhere to push to.
///
/// That sentence is the whole of push delivery. A subscriber is told each
/// transition once, by whichever of push and pull reaches it first, and the
/// cursor is what makes "once" true across both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscriber {
    pub cursor: u64,
    /// Last time this subscriber asked, or was pushed to. A cursor with no
    /// sign of life for `SUBSCRIBER_RETENTION` is dropped.
    pub seen_at: DateTime<Utc>,
    /// Where a running session is listening, while one is. Absent means pull
    /// only — which is every subscriber that predates `inkling subscribe`,
    /// and every session that has since ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<SessionAddress>,
    /// When messages were pushed here, oldest first, kept only for
    /// `PUSH_WINDOW`. What `allowance` counts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pushes: Vec<DateTime<Utc>>,
}

impl Watches {
    /// Start watching on `owner`'s behalf, or extend a watch already running.
    ///
    /// Re-watching keeps the baseline: resetting it would swallow every
    /// transition since the original subscription, which is exactly what the
    /// caller was asking not to miss.
    ///
    /// The furthest deadline wins, so one session's two-hour interest can't
    /// cut another session's day short. Registering interest also makes the
    /// owner a reader, which is what gives it a stream to be told about.
    pub fn watch(
        &mut self,
        reference: &Reference,
        owner: &str,
        deadline: DateTime<Utc>,
        now: DateTime<Utc>,
    ) {
        self.meet(owner, now);
        let entry = self
            .watches
            .entry(reference.to_string())
            .or_insert_with(|| Watch {
                reference: reference.clone(),
                since: now,
                deadline,
                seen: None,
                owners: BTreeSet::new(),
                stirred_at: None,
            });
        entry.deadline = entry.deadline.max(deadline);
        entry.owners.insert(owner.to_string());
        entry.stirred_at = Some(now);
    }

    /// Withdraw `owner`'s interest, and remove the watch once nobody is left.
    pub fn unwatch(&mut self, reference: &Reference, owner: &str) -> Unwatched {
        let key = reference.to_string();
        let Some(watch) = self.watches.get_mut(&key) else {
            return Unwatched::Absent;
        };
        if !watch.owners.remove(owner) {
            return Unwatched::Absent;
        }
        match watch.owners.is_empty() {
            true => {
                self.watches.remove(&key);
                Unwatched::Removed
            }
            false => Unwatched::StillWatched,
        }
    }

    /// Stop watching everything of `owner`'s. Reports how many watches the
    /// caller stopped watching, not how many were removed.
    pub fn unwatch_all(&mut self, owner: &str) -> usize {
        let references: Vec<Reference> = self
            .watches
            .values()
            .filter(|watch| watch.owners.contains(owner))
            .map(|watch| watch.reference.clone())
            .collect();
        for reference in &references {
            self.unwatch(reference, owner);
        }
        references.len()
    }

    /// Whether this reader exists at all.
    ///
    /// `--subscriber` creates one that doesn't — "keep my place" starting now
    /// is the right answer for a session nobody has met. A `--since` token is
    /// the opposite claim: it names a place in a stream, and a stream that has
    /// been collected is an error rather than a fresh start.
    pub fn knows(&self, owner: &str) -> bool {
        self.subscribers.contains_key(owner)
    }

    /// Whether anything could still be pushed to this subscriber: a live
    /// watch of its own.
    pub fn has_live_watch(&self, subscriber: &str, now: DateTime<Utc>) -> bool {
        self.watches
            .values()
            .any(|watch| watch.deadline > now && watch.owners.contains(subscriber))
    }

    /// Whether anything can still reach this reader: somewhere to push to, a
    /// live watch to produce news, or news already owed.
    ///
    /// A reader with none of the three is a cursor and nothing else. Its record
    /// stays until `SUBSCRIBER_RETENTION` — a resumed session keeps its id, and
    /// a line of JSON is cheap — but there is nothing to *say* about it, which
    /// is why `inkling status` counts these rather than listing them. A watch it
    /// registers later starts it at the head in any case.
    pub fn reachable(&self, subscriber: &str, now: DateTime<Utc>) -> bool {
        let Some(entry) = self.subscribers.get(subscriber) else {
            return false;
        };
        entry.address.is_some()
            || self.has_live_watch(subscriber, now)
            || !crate::domain::watch::addressed(&self.events, entry.cursor, subscriber).is_empty()
    }

    /// The subscriber listening on this socket, if one is.
    ///
    /// How a watch registered from inside a session finds its owner without
    /// being told: the session's Bash carries the socket in its environment,
    /// and the subscriber attached to it is the session that will be pushed
    /// to. That is the identity that matters, rather than whatever id the
    /// caller's own process thinks it has.
    pub fn subscriber_at(&self, socket: &Path) -> Option<&str> {
        self.attached()
            .into_iter()
            .find(|(_, address, _)| address.path() == socket)
            .map(|(subscriber, _, _)| subscriber)
    }

    /// What is still worth asking about.
    pub fn active(&self, now: DateTime<Utc>) -> Vec<Reference> {
        self.watches
            .values()
            .filter(|watch| watch.deadline > now)
            .map(|watch| watch.reference.clone())
            .collect()
    }

    /// Whether any watch is due a reading, i.e. whether the next reader should
    /// pay for a poll before answering. `floor` is the youngest reading the
    /// caller will pay to replace; the schedule decides the rest.
    ///
    /// A watch never read is due immediately; with no active watches nothing
    /// is due, which is what keeps a hook that watches nothing free.
    pub fn due(&self, now: DateTime<Utc>, floor: TimeDelta) -> bool {
        !self.due_now(now, floor).is_empty()
    }

    /// The batch for a poll: every live watch whose reading has come due.
    pub fn due_now(&self, now: DateTime<Utc>, floor: TimeDelta) -> Vec<Reference> {
        self.watches
            .values()
            .filter(|watch| watch.deadline > now && watch.next_read(floor) <= now)
            .map(|watch| watch.reference.clone())
            .collect()
    }

    /// When the next live watch comes due, which is how long a poller with
    /// nothing else to do may sleep.
    pub fn next_read(&self) -> Option<DateTime<Utc>> {
        self.watches
            .values()
            .map(|watch| watch.next_read(TimeDelta::zero()))
            .min()
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
            // Who is told is decided here, once, from who was watching when
            // it moved — not looked up at read time, by when the watch may be
            // finished and its owners gone.
            let audience = watch.owners.clone();
            let read = reading(watch.seen.as_ref().map(|s| &s.standing), sighting);
            if !read.moved.is_empty() {
                watch.stirred_at = Some(observed_at);
            }
            for moved in read.moved {
                self.last_seq += 1;
                tracing::info!(
                    reference = %reference,
                    axis = moved.axis.as_str(),
                    from = moved.from,
                    to = moved.to,
                    seq = self.last_seq,
                    audience = audience.len(),
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
                    job: moved.job,
                    audience: audience.clone(),
                });
            }
            let finished = read.standing.is_finished();
            watch.seen = Some(Seen {
                at: observed_at,
                standing: read.standing,
                title: sighting.title.clone(),
            });
            if finished {
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
        let cursor = self.meet(subscriber, now).cursor;

        let due: Vec<Event> = crate::domain::watch::addressed(&self.events, cursor, subscriber)
            .into_iter()
            .cloned()
            .collect();
        if let Some(entry) = self.subscribers.get_mut(subscriber) {
            entry.cursor = head;
        }
        // The third corner of the triangle that answers "I never got told":
        // transitions detected, pushes written, events pulled. At info when
        // it carried something, because that is the line somebody will come
        // looking for — and at debug when it didn't, because that is every
        // prompt of every session and would be the whole log.
        match due.is_empty() {
            true => tracing::debug!(subscriber, cursor = cursor, "events read, nothing owed"),
            false => tracing::info!(
                subscriber,
                cursor_before = cursor,
                cursor_after = head,
                returned = due.len(),
                "events read"
            ),
        }
        due
    }

    /// The subscriber's record, created at the head of the log if this is the
    /// first anyone has heard of it.
    ///
    /// Starting at the head rather than at zero is the baseline rule one level
    /// up: a session met for the first time doesn't inherit transitions
    /// another session already acted on.
    fn meet(&mut self, subscriber: &str, now: DateTime<Utc>) -> &mut Subscriber {
        let head = self.last_seq;
        let entry = self
            .subscribers
            .entry(subscriber.to_string())
            .or_insert(Subscriber {
                cursor: head,
                seen_at: now,
                address: None,
                pushes: Vec::new(),
            });
        entry.seen_at = now;
        entry
    }

    /// Attach somewhere to push to. Reports whether it replaced an address
    /// that was already there.
    ///
    /// Re-subscribing keeps the cursor: `claude --resume` keeps the session
    /// id but gets a new process and a new socket, and what that session
    /// wants is the net change since it left — not a replay of it, and not
    /// silence about it.
    pub fn attach(
        &mut self,
        subscriber: &str,
        address: SessionAddress,
        now: DateTime<Utc>,
    ) -> bool {
        self.meet(subscriber, now)
            .address
            .replace(address)
            .is_some()
    }

    /// Stop pushing to a subscriber, and nothing else.
    ///
    /// The cursor and everything past it stay, so a session that ends with
    /// unread news can still pull it on its way back in. Reports whether
    /// there was an address to detach.
    pub fn detach(&mut self, subscriber: &str) -> bool {
        self.subscribers
            .get_mut(subscriber)
            .and_then(|entry| entry.address.take())
            .is_some()
    }

    /// Every subscriber a push could reach, with where its cursor sits.
    pub fn attached(&self) -> Vec<(&str, &SessionAddress, u64)> {
        self.subscribers
            .iter()
            .filter_map(|(id, entry)| {
                let address = entry.address.as_ref()?;
                Some((id.as_str(), address, entry.cursor))
            })
            .collect()
    }

    /// Move a cursor up to a sequence number that was successfully pushed.
    ///
    /// Forwards only. A prompt that pulled while the push was in flight has
    /// already moved it, and winding it back would tell that session the same
    /// thing twice — which is the one promise this feed makes.
    pub fn pushed(&mut self, subscriber: &str, seq: u64, sent: usize, now: DateTime<Utc>) {
        if let Some(entry) = self.subscribers.get_mut(subscriber) {
            entry.cursor = entry.cursor.max(seq);
            entry.seen_at = now;
            entry.pushes.retain(|at| now - *at < PUSH_WINDOW);
            entry.pushes.extend(std::iter::repeat_n(now, sent));
        }
    }

    /// How many more messages this session may be sent right now.
    ///
    /// Zero is not a failure and nothing is dropped: the transitions stay in
    /// the feed, addressed to this reader, and the next prompt takes them.
    pub fn allowance(&self, subscriber: &str, now: DateTime<Utc>) -> usize {
        let recent = self.subscribers.get(subscriber).map_or(0, |entry| {
            entry
                .pushes
                .iter()
                .filter(|at| now - **at < PUSH_WINDOW)
                .count()
        });
        PUSH_BUDGET.saturating_sub(recent)
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
            .retain(|_, subscriber| now - subscriber.seen_at < SUBSCRIBER_RETENTION);
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
    ///
    /// A file it cannot use reads as no file, because every caller here is on
    /// a path that must not fail — but it says so. Silently, "inkling has
    /// stopped noticing anything" and "nothing is being watched" looked
    /// identical, which is the one pair of states worth telling apart.
    pub fn read(&self) -> Watches {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Watches::default(),
            Err(error) => {
                tracing::warn!(%error, path = %self.path.display(), "cannot read the watch store");
                return Watches::default();
            }
        };
        match serde_json::from_str::<Watches>(&raw) {
            Ok(watches) if watches.version <= VERSION => watches,
            Ok(watches) => {
                tracing::warn!(
                    found = watches.version,
                    supported = VERSION,
                    "the watch store is from a newer version; reading it as empty"
                );
                Watches::default()
            }
            Err(error) => {
                tracing::warn!(%error, "the watch store is unreadable; reading it as empty");
                Watches::default()
            }
        }
    }

    /// Load, tidy, change, save — all under the lock, which is the only way
    /// anything in here is written.
    ///
    /// The wait for the lock is capped at `PATIENCE`, and a caller that loses
    /// is told so rather than queued. Every hook Claude Code runs comes through
    /// here, and a hook that waits on a lock is a session waiting on a lock;
    /// giving up costs one notification, which is the price this whole tool is
    /// allowed to charge.
    pub fn update<T>(
        &self,
        now: DateTime<Utc>,
        change: impl FnOnce(&mut Watches) -> T,
    ) -> Result<T, StoreError> {
        let lock_path = FileLock::path_for(&self.path);
        let _lock = FileLock::acquire_within(&lock_path, PATIENCE)
            .map_err(|e| StoreError::Write(e.to_string()))?
            .ok_or(StoreError::Busy {
                waited_ms: PATIENCE.as_millis() as u64,
            })?;

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

        // Owner-only from the moment it exists: this file carries session
        // tokens. `mode` covers the file this call creates; `set_permissions`
        // covers a temp left behind by an earlier, looser write.
        let temp = self.path.with_extension("tmp");
        let write = |temp: &Path| -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(OWNER_ONLY)
                .open(temp)?;
            file.set_permissions(std::fs::Permissions::from_mode(OWNER_ONLY))?;
            file.write_all(json.as_bytes())
        };
        write(&temp).map_err(|e| StoreError::Write(e.to_string()))?;
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

    /// A store with one watch, asked for by `owners`, baselined at `state`.
    fn watched_by(owners: &[&str], state: State) -> Watches {
        let mut watches = Watches::default();
        for owner in owners {
            watches.watch(&pr(3), owner, forever(), now());
        }
        watches.record(&[(pr(3), seen_pull(state, None))], now());
        watches
    }

    fn watching(state: State) -> Watches {
        watched_by(&["s-1"], state)
    }

    #[test]
    fn the_first_reading_baselines_without_an_event() {
        let mut watches = Watches::default();
        watches.watch(&pr(3), "s-1", forever(), now());

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
    fn a_push_costs_a_session_two_events_rather_than_three() {
        // The dogfood finding, at the level a session felt it: pushing a commit
        // put `checks success → none` and `none → pending` in the turn before
        // the `pending → success` that was the news. What is left is what a
        // poll that never saw the gap would have reported.
        let mut watches = watching(State::Open);
        watches.record(
            &[(pr(3), seen_pull(State::Open, Some(CheckState::Success)))],
            now(),
        );

        let moves: Vec<(String, String)> =
            [None, Some(CheckState::Pending), Some(CheckState::Success)]
                .into_iter()
                .flat_map(|checks| {
                    watches.record(&[(pr(3), seen_pull(State::Open, checks))], now())
                })
                .map(|event| (event.from, event.to))
                .collect();

        assert_eq!(
            moves,
            [
                ("success".to_string(), "pending".to_string()),
                ("pending".to_string(), "success".to_string())
            ]
        );
    }

    #[test]
    fn re_watching_keeps_the_baseline_and_never_shortens_the_deadline() {
        // Otherwise "make sure I'm still watching this" would quietly swallow
        // every transition since the original subscription — and a second
        // reader's shorter interest would cut the first reader's day short.
        let long = at("2026-09-21T12:00:00Z");
        let mut watches = Watches::default();
        watches.watch(&pr(3), "a", long, now());
        watches.record(&[(pr(3), seen_pull(State::Open, None))], now());

        watches.watch(&pr(3), "b", at("2026-09-20T13:00:00Z"), now());
        let watch = &watches.watches["github.com/dpep/inkling#3"];
        assert_eq!(watch.deadline, long, "the furthest deadline wins");
        assert!(watch.seen.is_some(), "still baselined at open");

        let events = watches.record(&[(pr(3), seen_pull(State::Merged, None))], now());
        assert_eq!(events.len(), 1, "one poll, one event");
        assert_eq!(events[0].audience.len(), 2, "addressed to both readers");
    }

    #[test]
    fn an_expired_watch_stops_being_polled_and_is_collected() {
        let mut watches = Watches::default();
        watches.watch(&pr(3), "s-1", at("2026-09-20T11:59:00Z"), now());
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
        let mut watches = watched_by(&["session-a"], State::Open);
        watches.take("session-a", now());
        watches.record(&[(pr(3), seen_pull(State::Approved, None))], now());

        let first = watches.take("session-a", now());
        assert_eq!(first.len(), 1, "delivered once");
        assert!(watches.take("session-a", now()).is_empty(), "and not again");
    }

    #[test]
    fn a_reader_hears_nothing_from_before_it_asked() {
        // Two rules doing one job: a reader meets the log at its head, and is
        // only ever the audience for what it asked about. Neither on its own
        // would keep one session's news out of another's.
        let mut watches = watched_by(&["session-a"], State::Open);
        watches.record(&[(pr(3), seen_pull(State::Approved, None))], now());

        assert!(
            watches.take("session-b", now()).is_empty(),
            "history belongs to whoever was there for it"
        );
        watches.watch(&pr(3), "session-b", forever(), now());
        watches.record(&[(pr(3), seen_pull(State::Merged, None))], now());

        assert_eq!(
            watches.take("session-b", now()).len(),
            1,
            "but the next one is its own"
        );
        assert_eq!(
            watches.take("session-a", now()).len(),
            2,
            "and reading b's stream never consumed a's"
        );
    }

    #[test]
    fn unwatching_stops_one_reader_and_leaves_the_other() {
        let mut watches = watched_by(&["a", "b"], State::Open);
        assert_eq!(watches.unwatch(&pr(3), "a"), Unwatched::StillWatched);
        assert_eq!(
            watches.active(now()).len(),
            1,
            "one poll still serves whoever is left"
        );

        watches.record(&[(pr(3), seen_pull(State::Approved, None))], now());
        assert!(watches.take("a", now()).is_empty(), "a stopped asking");
        assert_eq!(watches.take("b", now()).len(), 1);

        assert_eq!(watches.unwatch(&pr(3), "b"), Unwatched::Removed);
        assert!(watches.active(now()).is_empty());
        assert_eq!(
            watches.unwatch(&pr(3), "b"),
            Unwatched::Absent,
            "and again says so"
        );
    }

    #[test]
    fn two_readers_watching_one_reference_each_hear_it_once() {
        // One watch, one poll, two streams: the reason a watch keeps a set of
        // owners rather than one.
        let mut watches = watched_by(&["a", "b"], State::Open);
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
        watches.watch(&pr(3), "s-1", forever(), now());
        assert!(watches.due(now(), TimeDelta::seconds(60)), "never read");

        watches.record(&[(pr(3), seen_pull(State::Open, None))], now());
        assert!(!watches.due(now(), TimeDelta::seconds(60)), "just read");
        assert!(
            watches.due(now() + TimeDelta::seconds(60), TimeDelta::seconds(60)),
            "the reading aged out"
        );
    }

    #[test]
    fn the_quieter_a_watch_the_less_often_it_is_read() {
        let every = |hours: i64| read_every(TimeDelta::hours(hours));
        assert_eq!(
            read_every(TimeDelta::minutes(-5)),
            FASTEST_READ,
            "just touched"
        );
        assert_eq!(every(0), TimeDelta::minutes(1));
        assert_eq!(
            read_every(TimeDelta::minutes(24)),
            FASTEST_READ,
            "the floor lasts 24m"
        );
        assert_eq!(every(1), TimeDelta::seconds(150));
        assert_eq!(every(6), TimeDelta::minutes(15));
        assert_eq!(every(12), TimeDelta::minutes(30));
        assert_eq!(every(24), TimeDelta::minutes(60));
        assert_eq!(
            every(72),
            TimeDelta::minutes(60),
            "and a day is the ceiling"
        );
    }

    /// Registered at `now()`, then read quietly every time it came due for
    /// `quiet`, so its last reading is as old as the schedule allows.
    fn quiet_for(quiet: TimeDelta) -> Watches {
        let mut watches = Watches::default();
        watches.watch(&pr(3), "s-1", forever(), now());
        let mut t = now();
        while t < now() + quiet {
            watches.record(&[(pr(3), seen_pull(State::Open, None))], t);
            t = watches.next_read().unwrap();
        }
        watches
    }

    #[test]
    fn a_watch_quiet_for_six_hours_is_read_every_quarter_hour() {
        let watches = quiet_for(TimeDelta::hours(6));
        let seen = watches.watches["github.com/dpep/inkling#3"]
            .seen
            .as_ref()
            .unwrap()
            .at;
        let next = watches.next_read().unwrap();
        assert!(next - seen >= TimeDelta::minutes(14), "{}", next - seen);
        assert!(next - seen <= TimeDelta::minutes(16), "{}", next - seen);
        assert!(!watches.due(next - TimeDelta::seconds(1), TimeDelta::zero()));
        assert!(watches.due(next, TimeDelta::zero()));
    }

    #[test]
    fn three_quiet_days_cost_under_two_hundred_reads_not_four_thousand() {
        // The whole point, as a number: a once-a-minute schedule reads a 72h
        // watch 4,320 times.
        let mut watches = Watches::default();
        watches.watch(&pr(3), "s-1", forever(), now());
        let mut reads = 0;
        let mut t = now();
        while t < now() + WATCH_TTL {
            watches.record(&[(pr(3), seen_pull(State::Open, None))], t);
            reads += 1;
            t = watches.next_read().unwrap();
        }
        // 24 at the floor, 24·ln(60) ≈ 98 while backing off, then 48 hourly.
        assert!((160..=180).contains(&reads), "{reads}");
    }

    #[test]
    fn touching_a_quiet_watch_brings_it_back_to_every_minute() {
        // A three-day-old pull request just pushed to: its new CI is the news,
        // and hourly reads would deliver it up to an hour late.
        let mut watches = quiet_for(TimeDelta::hours(70));
        let t = watches.next_read().unwrap() - TimeDelta::minutes(30);
        assert!(
            !watches.due(t, TimeDelta::zero()),
            "hourly, before the touch"
        );

        watches.watch(&pr(3), "s-1", forever(), t);
        assert!(watches.due(t, TimeDelta::zero()), "due at once");
        watches.record(&[(pr(3), seen_pull(State::Open, None))], t);
        assert_eq!(watches.next_read(), Some(t + FASTEST_READ));
    }

    #[test]
    fn a_transition_resets_the_quiet() {
        let mut watches = quiet_for(TimeDelta::hours(30));
        let t = watches.next_read().unwrap();
        watches.record(&[(pr(3), seen_pull(State::Approved, None))], t);
        assert_eq!(watches.next_read(), Some(t + FASTEST_READ));
    }

    #[test]
    fn only_the_watches_that_are_due_are_read() {
        let mut watches = quiet_for(TimeDelta::hours(30));
        let t = watches.watches["github.com/dpep/inkling#3"]
            .seen
            .as_ref()
            .unwrap()
            .at
            + TimeDelta::minutes(5);
        watches.watch(&pr(4), "s-1", forever(), t);

        assert_eq!(watches.due_now(t, TimeDelta::zero()), [pr(4)]);
    }

    #[test]
    fn a_caller_may_ask_for_readings_no_fresher_than_it_needs() {
        // `events --refresh 5m`: the schedule says a minute, the caller says
        // it doesn't want to pay for anything younger than five.
        let mut watches = Watches::default();
        watches.watch(&pr(3), "s-1", forever(), now());
        watches.record(&[(pr(3), seen_pull(State::Open, None))], now());

        let later = now() + TimeDelta::minutes(2);
        assert!(watches.due(later, TimeDelta::zero()));
        assert!(!watches.due(later, TimeDelta::minutes(5)));
    }

    #[test]
    fn a_watch_from_before_the_schedule_is_quiet_since_it_was_registered() {
        // Files written before `stirred_at` existed.
        let mut watches = Watches::default();
        watches.watch(&pr(3), "s-1", forever(), at("2026-09-19T12:00:00Z"));
        let mut raw = serde_json::to_value(&watches.watches["github.com/dpep/inkling#3"]).unwrap();
        raw.as_object_mut().unwrap().remove("stirred_at");

        let watch: Watch = serde_json::from_value(raw).unwrap();
        assert_eq!(watch.quiet_since(), at("2026-09-19T12:00:00Z"));
    }

    #[test]
    fn an_expired_watch_does_not_keep_the_poller_busy() {
        let mut watches = Watches::default();
        watches.watch(
            &pr(3),
            "s-1",
            at("2026-09-20T11:00:00Z"),
            at("2026-09-19T11:00:00Z"),
        );
        assert!(!watches.due(now(), TimeDelta::seconds(60)));
    }

    #[test]
    fn old_events_age_out_without_taking_their_readers_with_them() {
        let mut watches = watched_by(&["quiet"], State::Open);
        watches.record(&[(pr(3), seen_pull(State::Approved, None))], now());
        watches.take("quiet", now());

        watches.collect_garbage(now() + RETENTION + TimeDelta::hours(1), RETENTION);
        assert!(
            watches.events.is_empty(),
            "a transition older than the retention is not news"
        );
        assert_eq!(
            watches.subscribers.len(),
            1,
            "a session waiting for something is silent by definition"
        );

        watches.collect_garbage(now() + TimeDelta::days(8), RETENTION);
        assert!(
            watches.subscribers.is_empty(),
            "but a week of nothing at all is gone"
        );
    }

    #[test]
    fn a_reader_nothing_can_reach_has_nothing_to_report() {
        // The leftover dogfooding found in `inkling status`: detached, watching
        // nothing, owed nothing. Its cursor is kept for a week in case the
        // session comes back, but it is a row that says nothing every time.
        let mut watches = Watches::default();
        watches.take("s-1", now());
        assert!(!watches.reachable("s-1", now()));

        watches.attach("s-1", address("/tmp/s"), now());
        assert!(watches.reachable("s-1", now()), "somewhere to push to");
        watches.detach("s-1");

        watches.watch(&pr(3), "s-1", at("2026-09-21T12:00:00Z"), now());
        assert!(watches.reachable("s-1", now()), "news can still happen");

        // Baselined, then a transition addressed to it, then it withdraws:
        // what it is owed is owed for as long as the log keeps it.
        watches.record(&[(pr(3), seen_pull(State::Open, None))], now());
        watches.record(&[(pr(3), seen_pull(State::Approved, None))], now());
        watches.unwatch(&pr(3), "s-1");
        assert!(watches.reachable("s-1", now()), "one event still owed");

        watches.take("s-1", now());
        assert!(!watches.reachable("s-1", now()), "and now nothing is");
    }

    fn address(path: &str) -> SessionAddress {
        SessionAddress::new(path, Some(crate::secret::Secret::new("tok_s3cret")))
    }

    #[test]
    fn re_subscribing_replaces_the_address_and_keeps_the_cursor() {
        // `claude --resume` keeps the session id and gets a new socket. The
        // returning session wants the net change since it left: replaying
        // what it was already told would be noise, and starting it over at
        // the head would lose what moved while it was away.
        let mut watches = watching(State::Open);
        watches.attach("s-1", address("/tmp/first"), now());
        watches.record(&[(pr(3), seen_pull(State::Approved, None))], now());

        let replaced = watches.attach("s-1", address("/tmp/second"), now());
        assert!(replaced, "it said so");

        let attached = watches.attached();
        assert_eq!(attached.len(), 1);
        assert_eq!(attached[0].1.path(), Path::new("/tmp/second"));
        assert_eq!(
            watches.take("s-1", now()).len(),
            1,
            "the transition it missed is still owed to it"
        );
    }

    #[test]
    fn detaching_stops_the_push_and_nothing_else() {
        // A session that ends with unread news gets it when it comes back,
        // or from any other reader of the same cursor.
        let mut watches = watching(State::Open);
        watches.attach("s-1", address("/tmp/s"), now());
        watches.record(&[(pr(3), seen_pull(State::Approved, None))], now());

        assert!(watches.detach("s-1"));
        assert!(watches.attached().is_empty(), "nowhere left to push");
        assert!(!watches.detach("s-1"), "and nothing left to detach");
        assert_eq!(
            watches.take("s-1", now()).len(),
            1,
            "the event is still pullable"
        );
    }

    #[test]
    fn a_push_and_a_pull_never_deliver_the_same_transition_twice() {
        // One cursor, moved by whichever got there first. The push here lands
        // while the prompt hook is mid-pull.
        let mut watches = watching(State::Open);
        watches.attach("s-1", address("/tmp/s"), now());
        watches.record(&[(pr(3), seen_pull(State::Approved, None))], now());

        let pushed = watches.attached()[0].2;
        let due = crate::domain::watch::since(&watches.events, pushed).to_vec();
        assert_eq!(due.len(), 1);
        watches.pushed("s-1", due[0].seq, 1, now());

        assert!(
            watches.take("s-1", now()).is_empty(),
            "the prompt does not repeat what the push delivered"
        );
    }

    #[test]
    fn a_late_push_does_not_wind_a_cursor_back() {
        // The other order: the prompt pulled while the push was in flight.
        let mut watches = watching(State::Open);
        watches.attach("s-1", address("/tmp/s"), now());
        watches.record(&[(pr(3), seen_pull(State::Approved, None))], now());
        watches.record(&[(pr(3), seen_pull(State::Merged, None))], now());

        assert_eq!(watches.take("s-1", now()).len(), 2, "pulled both");
        watches.pushed("s-1", 1, 1, now());
        assert!(
            watches.take("s-1", now()).is_empty(),
            "the second event must not come round again"
        );
    }

    #[test]
    fn an_address_survives_a_round_trip_through_the_file() {
        let store = WatchStore::new(scratch("address"));
        store
            .update(now(), |watches| {
                watches.attach("s-1", address("/tmp/s"), now())
            })
            .unwrap();

        let reloaded = store.read();
        let attached = reloaded.attached();
        assert_eq!(attached.len(), 1);
        assert_eq!(attached[0].0, "s-1");
        assert_eq!(attached[0].1.path(), Path::new("/tmp/s"));
    }

    #[test]
    fn the_file_is_readable_only_by_its_owner() {
        // It holds session tokens now.
        let path = scratch("permissions");
        let store = WatchStore::new(&path);
        store
            .update(now(), |watches| {
                watches.attach("s-1", address("/tmp/s"), now())
            })
            .unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, OWNER_ONLY, "{mode:o}");
    }

    #[test]
    fn a_stored_subscriber_never_writes_its_token_to_a_log() {
        let mut watches = Watches::default();
        watches.attach("s-1", address("/tmp/s"), now());
        let debug = format!("{watches:?}");
        assert!(!debug.contains("s3cret"), "{debug}");
    }

    #[test]
    fn an_issue_reports_its_status_moving() {
        let mut watches = Watches::default();
        watches.watch(&issue("AI-1"), "s-1", forever(), now());
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
                watches.watch(&pr(3), "s-1", forever(), now());
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
                watches.watch(&pr(3), "hook", forever(), now());
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
            .update(now(), |w| w.watch(&pr(3), "s-1", forever(), now()))
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

    #[test]
    fn a_writer_gives_up_on_a_held_lock_rather_than_queueing() {
        // Every hook Claude Code runs writes through here. Whatever is holding
        // the lock — a stopped process, a store on a stalled filesystem — the
        // hook has to come back, and the cost is one missed notification.
        let path = scratch("busy");
        let store = WatchStore::new(&path);
        let _held = FileLock::try_acquire(&FileLock::path_for(&path))
            .unwrap()
            .unwrap();

        let started = std::time::Instant::now();
        let refused = store.update(now(), |w| w.watch(&pr(3), "s-1", forever(), now()));

        assert!(
            matches!(refused, Err(StoreError::Busy { .. })),
            "{refused:?}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "{:?} is not a bound a prompt can pay",
            started.elapsed()
        );
    }
}
