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
use crate::domain::detect::Pushed;
use crate::domain::model::Reference;
use crate::domain::slack::owner;
use crate::domain::watch::{
    Axis, Event, Head, Reacted, Receipt, Receipts, Sighting, Standing, reading, replied,
};
use crate::ports::StoreError;
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The shape of the file. Bumped for *any* change to it, additive ones too:
/// an older build refuses a newer file rather than parse what it can and save
/// back less (docs/watching.md, "An older build meets a newer store").
/// `tests::stored_shape` stops compiling when the shape moves.
pub const VERSION: u32 = 3;

/// The reader a person at a terminal is. Every watch has an owner and every
/// read names one, so there is one rule rather than an owned path and an
/// unowned one. Its watches never pause: nothing says a terminal has gone.
pub const TERMINAL: &str = "cli";

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

/// The least time between two attempts to read a Slack message, whatever the
/// schedule says. Each attempt is a `claude -p` call that costs money, so the
/// floor counts attempts rather than successes: a bridge that keeps failing
/// must not be retried on every tick.
pub const SLACK_READ_FLOOR: TimeDelta = TimeDelta::minutes(5);

/// How many readings in a row must say a reference isn't there before its
/// watch ends. One can be wrong: a read that otherwise succeeded can leave a
/// message out, and a source can answer "not found" for an outage.
pub const GONE_AFTER: u32 = 3;

/// The least time between attempts to read this kind of reference.
fn attempt_floor(reference: &Reference) -> TimeDelta {
    match reference {
        Reference::SlackMessage { .. } | Reference::SlackChannel { .. } => SLACK_READ_FLOOR,
        _ => TimeDelta::zero(),
    }
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
    /// The branch a pull request's commits come from, once a reading or a
    /// lookup has named it. Kept on the watch rather than in a map of its own
    /// so it can't outlive the pull request it describes: a branch reused for
    /// a new pull request after this one's watch expires is looked up afresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<Head>,
    /// When a reading was last attempted, successful or not. Only a source
    /// with an attempt floor looks at it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tried_at: Option<DateTime<Utc>>,
    /// What each reader put into this conversation itself, as the hook saw
    /// it. Kept on the watch so it goes when the watch does.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub receipts: BTreeMap<String, Receipts>,
    /// Readings in a row that found the reference missing; a sighting resets
    /// it. Ends the watch at `GONE_AFTER`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub misses: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
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
        let scheduled = match &self.seen {
            None => self.since,
            Some(seen) => seen.at + read_every(seen.at - self.quiet_since()).max(floor),
        };
        match self.tried_at {
            Some(tried) => scheduled.max(tried + attempt_floor(&self.reference)),
            None => scheduled,
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
    /// Last time this subscriber asked, was pushed to, or answered a probe. A
    /// cursor with no sign of life for `SUBSCRIBER_RETENTION`, and nothing
    /// left to hear, is dropped.
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
    /// Had an address and lost it: a session that ended. Stored because a
    /// missing address can't say it — a session without the hooks never had
    /// one, and nothing will ever tell us it has gone. What pauses a watch.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub departed: bool,
    /// Detached because it asked to be. Unlike a probe or `SessionEnd`, that
    /// is not undone by the session's next hook — only by attaching.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unsubscribed: bool,
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
        // A sign of life, but not a way back from having ended: only `attach`
        // is, since only it gives `detach` an address to take away again.
        self.meet(owner, now);
        let entry = self
            .watches
            .entry(reference.key())
            .or_insert_with(|| Watch {
                reference: reference.clone(),
                since: now,
                deadline,
                seen: None,
                owners: BTreeSet::new(),
                stirred_at: None,
                head: None,
                tried_at: None,
                receipts: BTreeMap::new(),
                misses: 0,
            });
        // A channel first watched by id takes its name once one is known.
        if matches!(reference, Reference::SlackChannel { name: Some(_), .. }) {
            entry.reference = reference.clone();
        }
        entry.deadline = entry.deadline.max(deadline);
        entry.owners.insert(owner.to_string());
        entry.stirred_at = Some(now);
    }

    /// `owner`'s live watches on pull requests whose head is `branch` of
    /// `repo` on `host` — what a push there just changed.
    pub fn pushed_to(
        &self,
        owner: &str,
        host: &str,
        repo: &str,
        branch: &str,
        now: DateTime<Utc>,
    ) -> Vec<Reference> {
        self.watches
            .values()
            .filter(|watch| watch.deadline > now && watch.owners.contains(owner))
            .filter(|watch| {
                watch
                    .head
                    .as_ref()
                    .is_some_and(|head| head.is(repo, branch))
            })
            .filter(|watch| {
                matches!(&watch.reference, Reference::GitHubPullRequest { host: h, .. }
                    if h.eq_ignore_ascii_case(host))
            })
            .map(|watch| watch.reference.clone())
            .collect()
    }

    /// Touch `owner`'s watches on what a push updated, and return what was
    /// touched with the pushes nothing here knows about — the ones worth
    /// asking GitHub about.
    ///
    /// A branch GitHub offered to open a pull request for has none, so it is
    /// neither touched nor worth asking about.
    pub fn stir_pushed(
        &mut self,
        pushes: &[Pushed],
        owner: &str,
        deadline: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> (Vec<(Reference, String)>, Vec<Pushed>) {
        let mut stirred = Vec::new();
        let mut unknown = Vec::new();
        for push in pushes.iter().filter(|push| !push.invited) {
            let mine = self.pushed_to(owner, &push.host, &push.repository(), &push.branch, now);
            if mine.is_empty() {
                unknown.push(push.clone());
            }
            for reference in mine {
                self.watch(&reference, owner, deadline, now);
                stirred.push((reference, push.branch.clone()));
            }
        }
        (stirred, unknown)
    }

    /// Note what `owner` itself put into Slack, on each of its watches it
    /// could turn up in: a post on any watch of that conversation, a reaction
    /// on the watched message it was added to.
    pub fn receipt(&mut self, owner: &str, receipt: &Receipt, at: DateTime<Utc>) {
        for watch in self.watches.values_mut() {
            if !watch.owners.contains(owner) {
                continue;
            }
            let lands = match (receipt, &watch.reference) {
                (
                    Receipt::Posted { channel, .. },
                    Reference::SlackMessage { channel: on, .. }
                    | Reference::SlackChannel { channel: on, .. },
                ) => on == channel,
                (
                    Receipt::Reacted { channel, ts, .. },
                    Reference::SlackMessage {
                        channel: on,
                        ts: message,
                        ..
                    },
                ) => on == channel && message == ts,
                _ => false,
            };
            if !lands {
                continue;
            }
            let mine = watch.receipts.entry(owner.to_string()).or_default();
            match receipt {
                Receipt::Posted { ts, .. } => {
                    mine.posted.insert(ts.clone());
                }
                Receipt::Reacted { emoji, .. } => mine.reacted.push(Reacted {
                    emoji: emoji.clone(),
                    at,
                }),
            }
        }
    }

    /// Note that these were asked about at `at`, whatever came back.
    pub fn tried(&mut self, references: &[Reference], at: DateTime<Utc>) {
        for reference in references {
            if let Some(watch) = self.watches.get_mut(&reference.key()) {
                watch.tried_at = Some(at);
            }
        }
    }

    /// Note where a watched pull request's commits come from.
    pub fn remember_head(&mut self, reference: &Reference, head: Head) {
        if let Some(watch) = self.watches.get_mut(&reference.key()) {
            watch.head = Some(head);
        }
    }

    /// Withdraw `owner`'s interest, and remove the watch once nobody is left.
    pub fn unwatch(&mut self, reference: &Reference, owner: &str) -> Unwatched {
        let key = reference.key();
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

    /// Whether nobody left can hear this watch: every owner is a session that
    /// was attached and has since ended. Derived from the owners each time,
    /// so there is no flag on the watch to drift from them.
    ///
    /// Paused, not cancelled: the deadline still runs, the baseline is kept,
    /// and the session's return (`attach`) makes it due again — so a resumed
    /// session hears the net change, as it does for its cursor.
    ///
    /// A channel watch never pauses: it is a standing interest somebody asked
    /// for by name, not something a session happened to touch.
    pub fn paused(&self, watch: &Watch) -> bool {
        !matches!(watch.reference, Reference::SlackChannel { .. })
            && !watch.owners.is_empty()
            && watch.owners.iter().all(|owner| self.departed(owner))
    }

    /// Whether this reader has no address, and not by its own choice: what a
    /// hook from a live session attaches again.
    pub fn lost_address(&self, subscriber: &str) -> bool {
        subscriber != TERMINAL
            && self
                .subscribers
                .get(subscriber)
                .is_some_and(|entry| entry.address.is_none() && !entry.unsubscribed)
    }

    fn departed(&self, owner: &str) -> bool {
        owner != TERMINAL
            && self
                .subscribers
                .get(owner)
                .is_some_and(|entry| entry.departed)
    }

    fn paused_keys(&self) -> BTreeSet<String> {
        self.watches
            .iter()
            .filter(|(_, watch)| self.paused(watch))
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// What is still worth asking about.
    pub fn active(&self, now: DateTime<Utc>) -> Vec<Reference> {
        self.watches
            .values()
            .filter(|watch| watch.deadline > now && !self.paused(watch))
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
            .filter(|watch| {
                watch.deadline > now && !self.paused(watch) && watch.next_read(floor) <= now
            })
            .map(|watch| watch.reference.clone())
            .collect()
    }

    /// When the next live watch comes due, which is how long a poller with
    /// nothing else to do may sleep.
    pub fn next_read(&self) -> Option<DateTime<Utc>> {
        self.watches
            .values()
            .filter(|watch| !self.paused(watch))
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
            let key = reference.key();
            let heard = self
                .watches
                .get(&key)
                .and_then(|watch| self.heard(watch, sighting, observed_at));
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
            let stirred = !read.moved.is_empty();
            // A Slack reply is told to each reader on one watch, not all.
            let moved = match (heard, &read.standing) {
                (Some(heard), Standing::Message { replies, .. }) => {
                    let mut by_heard: BTreeMap<BTreeSet<String>, BTreeSet<String>> =
                        BTreeMap::new();
                    for (reader, new) in heard {
                        by_heard.entry(new).or_default().insert(reader);
                    }
                    let replied = by_heard.into_iter().filter_map(|(new, readers)| {
                        replied(replies, &new, &sighting.by).map(|moved| (moved, readers))
                    });
                    let rest = read.moved.into_iter().filter(|m| m.axis != Axis::Replies);
                    replied
                        .chain(rest.map(|moved| (moved, audience.clone())))
                        .collect()
                }
                _ => read
                    .moved
                    .into_iter()
                    .map(|moved| (moved, audience.clone()))
                    .collect::<Vec<_>>(),
            };
            if stirred || !moved.is_empty() {
                watch.stirred_at = Some(observed_at);
            }
            for (moved, audience) in moved {
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
                let receipt_for = audience
                    .iter()
                    .filter(|reader| moved.made.by(watch.receipts.get_mut(*reader)))
                    .cloned()
                    .collect();
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
                    receipt_for,
                    partial: moved.partial,
                    lines: moved.lines,
                    after: moved.after,
                    audience: audience.clone(),
                });
            }
            for mine in watch.receipts.values_mut() {
                mine.settle(observed_at);
            }
            watch.receipts.retain(|_, mine| !mine.is_empty());
            if sighting.head.is_some() {
                watch.head = sighting.head.clone();
            }
            watch.misses = 0;
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

    /// For a Slack message, the replies new to it that each of its readers is
    /// told on it.
    ///
    /// Every watch keeps every reply that answers it, so one coming or going
    /// loses or repeats nothing. A reader is told each reply once, on the
    /// latest of *their own* watched messages it answers, among those already
    /// read once — another session's watch never takes a reply from them. A
    /// first reading tells only what one of the reader's other watches has
    /// yet to see: it arrived since they last looked, and the baseline would
    /// swallow it. Anything else was there when they asked.
    fn heard(
        &self,
        watch: &Watch,
        sighting: &Sighting,
        now: DateTime<Utc>,
    ) -> Option<BTreeMap<String, BTreeSet<String>>> {
        let (
            Reference::SlackMessage {
                channel,
                ts,
                thread,
            },
            Standing::Message { replies, .. },
        ) = (&watch.reference, &sighting.standing)
        else {
            return None;
        };
        let held = |w: &Watch, reply: &str| {
            matches!(&w.seen, Some(Seen { standing: Standing::Message { replies, .. }, .. })
                if replies.contains_key(reply))
        };
        let root = thread.as_deref().unwrap_or(ts);
        let new: Vec<&String> = replies.keys().filter(|r| !held(watch, r)).collect();
        let heard = |reader: &String| {
            let theirs: Vec<&Watch> = self
                .watches
                .values()
                .filter(|w| {
                    matches!(w.reference, Reference::SlackMessage { .. })
                        && w.reference != watch.reference
                        && w.owners.contains(reader)
                        && w.seen.is_some()
                        && w.deadline > now
                        && !self.paused(w)
                })
                .collect();
            new.iter()
                .filter(|reply| {
                    let context = sighting.by.threaded.contains(**reply).then_some(root);
                    let answers =
                        |w: &Watch| owner(channel, reply, context, [&w.reference]).is_some();
                    let candidates = theirs.iter().map(|w| &w.reference);
                    let latest = owner(
                        channel,
                        reply,
                        context,
                        candidates.chain([&watch.reference]),
                    );
                    // The read keeps only what answers this message, so a reply
                    // no watch claims is still this one's.
                    latest.is_none_or(|latest| latest == ts)
                        && (watch.seen.is_some()
                            || theirs.iter().any(|w| answers(w) && !held(w, reply)))
                })
                .map(|reply| (*reply).clone())
                .collect()
        };
        Some(
            watch
                .owners
                .iter()
                .map(|reader| (reader.clone(), heard(reader)))
                .collect(),
        )
    }

    /// Count a reading that found each of these missing, and finish the
    /// watches that have been missing `GONE_AFTER` times in a row, each with
    /// one event saying so — the same end a merged pull request gets, so a
    /// deleted Slack message isn't paid for until its deadline.
    pub fn gone(&mut self, references: &[Reference], observed_at: DateTime<Utc>) -> Vec<Event> {
        let mut fresh = Vec::new();
        for reference in references {
            let key = reference.key();
            let Some(watch) = self.watches.get_mut(&key) else {
                continue;
            };
            watch.misses += 1;
            if watch.misses < GONE_AFTER {
                tracing::info!(reference = %reference, misses = watch.misses, "watch missing");
                continue;
            }
            let Some(watch) = self.watches.remove(&key) else {
                continue;
            };
            self.last_seq += 1;
            tracing::info!(reference = %reference, seq = self.last_seq, "watch gone");
            fresh.push(Event {
                seq: self.last_seq,
                at: observed_at,
                axis: Axis::of(&watch.reference),
                reference: watch.reference,
                from: "watched".into(),
                to: "gone".into(),
                title: watch.seen.map(|seen| seen.title).unwrap_or_default(),
                detail: None,
                job: None,
                receipt_for: BTreeSet::new(),
                partial: false,
                lines: 0,
                after: None,
                audience: watch.owners,
            });
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
                departed: false,
                unsubscribed: false,
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
    ///
    /// The same goes for its watches: any it paused on leaving are stirred,
    /// due at once, and read against the baseline they kept.
    pub fn attach(
        &mut self,
        subscriber: &str,
        address: SessionAddress,
        now: DateTime<Utc>,
    ) -> bool {
        let paused: Vec<String> = self
            .paused_keys()
            .into_iter()
            .filter(|key| self.watches[key].owners.contains(subscriber))
            .collect();
        let entry = self.meet(subscriber, now);
        entry.departed = false;
        entry.unsubscribed = false;
        let replaced = entry.address.replace(address).is_some();
        for key in paused {
            if let Some(watch) = self.watches.get_mut(&key) {
                watch.stirred_at = Some(now);
            }
        }
        replaced
    }

    /// Stop pushing to a subscriber, and nothing else.
    ///
    /// The cursor and everything past it stay, so a session that ends with
    /// unread news can still pull it on its way back in. Reports whether
    /// there was an address to detach.
    pub fn detach(&mut self, subscriber: &str) -> bool {
        let Some(entry) = self.subscribers.get_mut(subscriber) else {
            return false;
        };
        let had = entry.address.take().is_some();
        entry.departed |= had;
        had
    }

    /// Detach because the session asked to, which its own hooks won't undo.
    pub fn unsubscribe(&mut self, subscriber: &str) -> bool {
        let detached = self.detach(subscriber);
        if let Some(entry) = self.subscribers.get_mut(subscriber) {
            entry.unsubscribed = true;
        }
        detached
    }

    /// Detach, but only from `address`: a session that resumed onto a new
    /// socket since that one was found gone is still here.
    pub fn detach_from(&mut self, subscriber: &str, address: &SessionAddress) -> bool {
        let current = self
            .subscribers
            .get(subscriber)
            .and_then(|entry| entry.address.as_ref());
        match current == Some(address) {
            true => self.detach(subscriber),
            false => false,
        }
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

    /// A probe found this subscriber listening: a sign of life.
    pub fn answered(&mut self, subscriber: &str, now: DateTime<Utc>) {
        if let Some(entry) = self.subscribers.get_mut(subscriber) {
            entry.seen_at = now;
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
        // Quiet is only a reason to forget a reader with nothing left to
        // hear. One that owns a watch or is owed news keeps its cursor (and a
        // departed one its departure, or that watch would quietly resume).
        let (watches, events) = (&self.watches, &self.events);
        self.subscribers.retain(|id, subscriber| {
            now - subscriber.seen_at < SUBSCRIBER_RETENTION
                || watches.values().any(|w| w.owners.contains(id))
                || !crate::domain::watch::addressed(events, subscriber.cursor, id).is_empty()
        });
        Pruned {
            watches: before.0 - self.watches.len(),
            events: before.1 - self.events.len(),
            subscribers: before.2 - self.subscribers.len(),
        }
    }
}

/// Say when a watch pauses or resumes — once, at the change that did it,
/// rather than on every tick it spends paused.
fn log_pauses(before: &BTreeSet<String>, watches: &Watches) {
    let after = watches.paused_keys();
    for key in after.difference(before) {
        tracing::info!(reference = %key, "watch paused; every reader has gone");
    }
    for key in before.difference(&after) {
        if watches.watches.contains_key(key) {
            tracing::info!(reference = %key, "watch resumed");
        }
    }
}

enum Unparsed {
    /// Written by a newer build. Never "corrupt": what this build can't parse
    /// in it is that build's, so the file is left exactly as it is.
    Newer(u32),
    Broken(serde_json::Error),
}

/// The version first, then the rest: a newer file may hold variants this
/// build has never heard of, and failing on those would read as damage.
fn parse(raw: &str) -> Result<Watches, Unparsed> {
    #[derive(Deserialize)]
    struct Version {
        #[serde(default)]
        version: u32,
    }
    let found = serde_json::from_str::<Version>(raw)
        .map_err(Unparsed::Broken)?
        .version;
    if found > VERSION {
        return Err(Unparsed::Newer(found));
    }
    let mut watches: Watches = serde_json::from_str(raw).map_err(Unparsed::Broken)?;
    // A channel was keyed by its name before 2026-11-17, so one may be there
    // under both keys.
    for watch in std::mem::take(&mut watches.watches).into_values() {
        match watches.watches.entry(watch.reference.key()) {
            Entry::Vacant(slot) => {
                slot.insert(watch);
            }
            Entry::Occupied(mut slot) => merge(slot.get_mut(), watch),
        }
    }
    Ok(watches)
}

/// One watch of the same thing from two: every reader and receipt, the later
/// deadline and reading, and the reference that knows a name.
fn merge(into: &mut Watch, other: Watch) {
    if matches!(
        other.reference,
        Reference::SlackChannel { name: Some(_), .. }
    ) {
        into.reference = other.reference;
    }
    into.since = into.since.min(other.since);
    into.deadline = into.deadline.max(other.deadline);
    if other.seen.as_ref().map(|s| s.at) > into.seen.as_ref().map(|s| s.at) {
        into.seen = other.seen;
    }
    into.owners.extend(other.owners);
    into.stirred_at = into.stirred_at.max(other.stirred_at);
    into.head = into.head.take().or(other.head);
    into.tried_at = into.tried_at.max(other.tried_at);
    for (owner, theirs) in other.receipts {
        let mine = into.receipts.entry(owner).or_default();
        mine.posted.extend(theirs.posted);
        mine.reacted.extend(theirs.reacted);
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
        self.read_current().unwrap_or_default()
    }

    /// `Watches::lost_address` for a hook from a session listening on
    /// `socket`. Every tool call asks, so a store that already names the
    /// socket — the session is attached — answers without being parsed.
    pub fn lost_address(&self, subscriber: &str, socket: &Path) -> bool {
        let Ok(raw) = std::fs::read_to_string(&self.path) else {
            return false;
        };
        if let Ok(quoted) = serde_json::to_string(socket)
            && raw.contains(&quoted)
        {
            return false;
        }
        parse(&raw).is_ok_and(|watches| watches.lost_address(subscriber))
    }

    /// `read`, except that a newer build's file is `FutureVersion` rather than
    /// empty: for a caller that reports what the store holds, where empty
    /// would tell someone to watch everything again.
    pub fn read_current(&self) -> Result<Watches, StoreError> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Watches::default()),
            Err(error) => {
                tracing::warn!(%error, path = %self.path.display(), "cannot read the watch store");
                return Ok(Watches::default());
            }
        };
        match parse(&raw) {
            Ok(watches) => Ok(watches),
            Err(Unparsed::Newer(found)) => {
                tracing::warn!(
                    found,
                    supported = VERSION,
                    "the watch store is from a newer build; reading it as empty"
                );
                Err(StoreError::FutureVersion { found })
            }
            Err(Unparsed::Broken(error)) => {
                tracing::warn!(%error, "the watch store is unreadable; reading it as empty");
                Ok(Watches::default())
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
        let paused = watches.paused_keys();
        let outcome = change(&mut watches);
        watches.version = VERSION;
        self.save(&watches)?;
        log_pauses(&paused, &watches);
        Ok(outcome)
    }

    fn load(&self) -> Result<Watches, StoreError> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Watches::default()),
            Err(e) => return Err(StoreError::Read(e.to_string())),
        };
        match parse(&raw) {
            Ok(watches) => Ok(watches),
            Err(Unparsed::Newer(found)) => Err(StoreError::FutureVersion { found }),
            // Subscriptions and cursors can't be rebuilt from anywhere else,
            // so the unreadable file is kept for a look rather than replaced.
            Err(Unparsed::Broken(error)) => {
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

    #[test]
    fn a_channel_by_id_and_by_name_is_one_watch_and_either_stops_it() {
        let channel = |name: Option<&str>| Reference::SlackChannel {
            channel: "C0E48AHSN".into(),
            name: name.map(Into::into),
        };
        let mut watches = Watches::default();
        watches.watch(&channel(None), "s", forever(), now());
        watches.watch(&channel(Some("general")), "s", forever(), now());
        assert_eq!(watches.watches.len(), 1);
        let watch = watches.watches.values().next().unwrap();
        assert_eq!(
            watch.reference.to_string(),
            "slack:#general",
            "named once known"
        );

        assert_eq!(watches.unwatch(&channel(None), "s"), Unwatched::Removed);
    }

    fn seen_pull(state: State, checks: Option<CheckState>) -> Sighting {
        Sighting {
            discussion: None,
            standing: Standing::PullRequest {
                state,
                checks,
                comments: None,
            },
            title: "Add inkling wait".into(),
            failing: None,
            head: None,
            by: Default::default(),
        }
    }

    fn seen_issue(status: &str, category: Category) -> Sighting {
        Sighting {
            discussion: None,
            standing: Standing::Issue {
                status: status.into(),
                category,
            },
            title: "Try the watch feature".into(),
            failing: None,
            head: None,
            by: Default::default(),
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

    fn slack() -> Reference {
        Reference::SlackMessage {
            channel: "C0123ABCD".into(),
            ts: "1786596050.245879".into(),
            thread: None,
        }
    }

    fn seen_message() -> Sighting {
        Sighting {
            discussion: None,
            standing: Standing::Message {
                reactions: Default::default(),
                replies: Default::default(),
            },
            title: String::new(),
            failing: None,
            head: None,
            by: Default::default(),
        }
    }

    #[test]
    fn a_slack_message_is_read_no_more_than_every_five_minutes_even_when_touched() {
        // Every read is a paid `claude -p` call; the schedule's one-minute
        // floor is for GitHub, where a read is one batched query.
        let mut watches = Watches::default();
        watches.watch(&slack(), "s-1", forever(), now());
        assert!(watches.due(now(), TimeDelta::zero()), "never read");
        watches.tried(&[slack()], now());
        watches.record(&[(slack(), seen_message())], now());

        let later = now() + TimeDelta::minutes(2);
        watches.watch(&slack(), "s-1", forever(), later);
        assert!(
            !watches.due(later, TimeDelta::zero()),
            "a touch doesn't beat the floor"
        );
        assert_eq!(watches.next_read(), Some(now() + SLACK_READ_FLOOR));
    }

    #[test]
    fn a_slack_read_that_keeps_failing_waits_out_the_floor_too() {
        // Never baselined, so the schedule says due — but each attempt cost
        // as much as a success would have.
        let mut watches = Watches::default();
        watches.watch(&slack(), "s-1", forever(), now());
        watches.tried(&[slack()], now());
        assert!(!watches.due(now() + TimeDelta::minutes(4), TimeDelta::zero()));
        assert!(watches.due(now() + SLACK_READ_FLOOR, TimeDelta::zero()));
    }

    #[test]
    fn an_attempt_floor_is_slack_s_alone() {
        let mut watches = Watches::default();
        watches.watch(&pr(3), "s-1", forever(), now());
        watches.tried(&[pr(3)], now());
        assert!(
            watches.due(now(), TimeDelta::zero()),
            "a failed github read retries next tick"
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

    fn head(repo: &str, branch: &str) -> Head {
        Head {
            repo: repo.into(),
            branch: branch.into(),
        }
    }

    #[test]
    fn a_reading_remembers_where_a_pull_requests_commits_come_from() {
        let mut watches = Watches::default();
        watches.watch(&pr(3), "s-1", forever(), now());
        let sighting = Sighting {
            head: Some(head("dpep/inkling", "push-watch")),
            ..seen_pull(State::Open, None)
        };
        watches.record(&[(pr(3), sighting)], now());
        assert_eq!(
            watches.pushed_to("s-1", "github.com", "dpep/inkling", "push-watch", now()),
            [pr(3)]
        );

        // A reading that can't name it (a deleted fork) forgets nothing.
        watches.record(&[(pr(3), seen_pull(State::Open, None))], now());
        assert!(watches.watches["github.com/dpep/inkling#3"].head.is_some());
    }

    #[test]
    fn a_push_matches_only_this_readers_live_watch_on_that_exact_head() {
        let mut watches = Watches::default();
        watches.watch(&pr(3), "s-1", forever(), now());
        watches.remember_head(&pr(3), head("dpep/inkling", "push-watch"));
        let matches = |owner: &str, host: &str, repo: &str, branch: &str, t| {
            watches.pushed_to(owner, host, repo, branch, t)
        };

        assert_eq!(
            matches("s-1", "github.com", "DPEP/Inkling", "push-watch", now()),
            [pr(3)]
        );
        for (owner, host, repo, branch, t, why) in [
            (
                "s-2",
                "github.com",
                "dpep/inkling",
                "push-watch",
                now(),
                "another session's",
            ),
            (
                "s-1",
                "github.com",
                "stranger/inkling",
                "push-watch",
                now(),
                "a fork's branch of the same name",
            ),
            (
                "s-1",
                "github.com",
                "dpep/inkling",
                "Push-Watch",
                now(),
                "branches are case-sensitive",
            ),
            (
                "s-1",
                "github.example.com",
                "dpep/inkling",
                "push-watch",
                now(),
                "another host",
            ),
            (
                "s-1",
                "github.com",
                "dpep/inkling",
                "push-watch",
                forever(),
                "expired",
            ),
        ] {
            assert!(matches(owner, host, repo, branch, t).is_empty(), "{why}");
        }
    }

    fn pushed(branch: &str, invited: bool) -> Pushed {
        Pushed {
            host: "github.com".into(),
            owner: "dpep".into(),
            repo: "inkling".into(),
            branch: branch.into(),
            invited,
        }
    }

    #[test]
    fn a_push_to_a_watched_head_touches_it_and_anything_else_is_left_to_ask_about() {
        let mut watches = Watches::default();
        watches.watch(&pr(3), "s-1", now() + TimeDelta::hours(8), now());
        watches.remember_head(&pr(3), head("dpep/inkling", "known"));
        let later = now() + TimeDelta::hours(6);

        let (stirred, unknown) = watches.stir_pushed(
            &[
                pushed("known", false),
                pushed("other", false),
                pushed("brand-new", true),
            ],
            "s-1",
            forever(),
            later,
        );
        assert_eq!(stirred, [(pr(3), "known".to_string())]);
        let watch = &watches.watches["github.com/dpep/inkling#3"];
        assert_eq!(watch.stirred_at, Some(later), "back on one-minute readings");
        assert_eq!(watch.deadline, forever(), "and its deadline extended");
        assert_eq!(
            unknown,
            [pushed("other", false)],
            "GitHub's offer to open one means there is none to look up"
        );
    }

    #[test]
    fn a_watch_from_before_heads_were_kept_still_loads() {
        let mut watches = Watches::default();
        watches.watch(&pr(3), "s-1", forever(), now());
        let raw = serde_json::to_value(&watches.watches["github.com/dpep/inkling#3"]).unwrap();
        assert!(
            raw.get("head").is_none(),
            "nothing written until it is known"
        );
        let watch: Watch = serde_json::from_value(raw).unwrap();
        assert_eq!(watch.head, None);
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

        watches.unwatch(&pr(3), "quiet");
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
                checks: None,
                comments: None,
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

    /// A tripwire rather than a test: it stops compiling when a stored type
    /// gains a field or a variant. When it does, bump `VERSION`, then the
    /// pattern. (`SessionAddress` keeps its fields private; it changes rarely.)
    #[allow(dead_code)]
    fn stored_shape(file: Watches, standing: Standing, axis: Axis, reference: Reference) {
        const _: () = assert!(VERSION == 3, "update the patterns below for this version");
        let Watches {
            version: _,
            watches,
            events,
            last_seq: _,
            subscribers,
        } = file;
        for (_, watch) in watches {
            let Watch {
                reference: _,
                since: _,
                deadline: _,
                seen,
                owners: _,
                stirred_at: _,
                head,
                tried_at: _,
                receipts,
                misses: _,
            } = watch;
            for (_, receipt) in receipts {
                let crate::domain::watch::Receipts { posted: _, reacted } = receipt;
                for crate::domain::watch::Reacted { emoji: _, at: _ } in reacted {}
            }
            let _ = seen.map(
                |Seen {
                     at: _,
                     standing: _,
                     title: _,
                 }| (),
            );
            let _ = head.map(|crate::domain::watch::Head { repo: _, branch: _ }| ());
        }
        for event in events {
            let Event {
                seq: _,
                at: _,
                reference: _,
                axis: _,
                from: _,
                to: _,
                title: _,
                detail: _,
                job: _,
                receipt_for: _,
                partial: _,
                lines: _,
                after: _,
                audience: _,
            } = event;
        }
        for (_, subscriber) in subscribers {
            let Subscriber {
                cursor: _,
                seen_at: _,
                address: _,
                pushes: _,
                departed: _,
                unsubscribed: _,
            } = subscriber;
        }
        match standing {
            Standing::PullRequest {
                state,
                checks,
                comments,
            } => {
                match state {
                    State::Closed
                    | State::Draft
                    | State::Open
                    | State::Approved
                    | State::Merged => {}
                }
                match checks {
                    Some(CheckState::Failure | CheckState::Pending | CheckState::Success)
                    | None => {}
                }
                let _ = comments.map(
                    |crate::domain::watch::Comments {
                         count: _,
                         latest: _,
                         at_latest: _,
                     }| (),
                );
            }
            Standing::Issue {
                status: _,
                category: Category::New | Category::Indeterminate | Category::Done,
            } => {}
            Standing::Message {
                reactions: _,
                replies: _,
            } => {}
            Standing::Channel {
                count: _,
                newest: _,
                arrived: _,
                more: _,
            } => {}
        }
        match axis {
            Axis::State
            | Axis::Checks
            | Axis::Status
            | Axis::Replies
            | Axis::Reactions
            | Axis::Comments
            | Axis::Messages => {}
        }
        match reference {
            Reference::GitHubPullRequest {
                host: _,
                owner: _,
                repo: _,
                number: _,
            }
            | Reference::JiraIssue { key: _ }
            | Reference::SlackMessage {
                channel: _,
                ts: _,
                thread: _,
            }
            | Reference::SlackChannel {
                channel: _,
                name: _,
            } => {}
        }
    }

    #[test]
    fn an_older_build_refuses_a_newer_store_rather_than_starting_over() {
        // Found live: a build from before channel watches met a store with
        // one, called it corrupt, and replaced every watch and cursor with
        // nothing. What it can't parse in a newer file is the newer build's.
        let path = scratch("newer-shape");
        let newer = format!(
            r#"{{"version": {}, "watches": {{"x": {{"reference": {{"kind": "carrier_pigeon"}}}}}}, "last_seq": 42}}"#,
            VERSION + 1
        );
        std::fs::write(&path, &newer).unwrap();
        let store = WatchStore::new(&path);

        let refused = store.update(now(), |w| w.take("s-1", now()));
        assert!(
            matches!(refused, Err(StoreError::FutureVersion { .. })),
            "{refused:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), newer, "untouched");
        assert!(!path.with_extension("corrupt-0").exists());
        assert!(store.read().watches.is_empty(), "and reads as nothing");
        assert!(
            matches!(store.read_current(), Err(StoreError::FutureVersion { found }) if found == VERSION + 1),
            "except to a caller that reports what it holds"
        );
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

    /// `owner` subscribed and then ended: the only way a reader departs.
    fn ended(watches: &mut Watches, owner: &str) {
        watches.attach(owner, address("/tmp/s"), now());
        watches.detach(owner);
    }

    fn is_live(watches: &Watches, at: DateTime<Utc>) -> bool {
        watches.due_now(at, TimeDelta::zero()) == [pr(3)]
            && watches.active(at) == [pr(3)]
            && watches.next_read().is_some()
    }

    #[test]
    fn a_watch_pauses_when_every_reader_has_ended() {
        let mut watches = watching(State::Open);
        let later = now() + TimeDelta::hours(2);
        assert!(is_live(&watches, later));

        ended(&mut watches, "s-1");
        assert!(watches.paused(&watches.watches["github.com/dpep/inkling#3"]));
        assert!(
            watches.due_now(later, TimeDelta::zero()).is_empty(),
            "not read"
        );
        assert!(watches.active(later).is_empty());
        assert_eq!(watches.next_read(), None, "nothing to wake a server for");
        assert_eq!(watches.watches.len(), 1, "paused, not cancelled");
    }

    #[test]
    fn a_watch_with_a_reader_we_cannot_see_leave_never_pauses() {
        let later = now() + TimeDelta::hours(2);
        // The terminal has no session to end; a session without the hooks
        // never had an address, so nothing says it has gone. Either one keeps
        // a watch live beside a session that has ended — as does a session
        // still attached.
        for other in [TERMINAL, "no-hooks", "attached"] {
            let mut watches = watched_by(&["s-1", other], State::Open);
            if other == "attached" {
                watches.attach(other, address("/tmp/a"), now());
            }
            ended(&mut watches, "s-1");
            assert!(is_live(&watches, later), "{other} still hears it");
        }

        let mut watches = watched_by(&["no-hooks"], State::Open);
        watches.detach("no-hooks");
        assert!(is_live(&watches, later), "detaching nothing is not ending");
    }

    #[test]
    fn a_resumed_session_hears_the_net_change_since_it_left() {
        // Quiet for a day, so it is on hourly readings: only the return can
        // make it due five minutes after its last one.
        let mut watches = Watches::default();
        watches.watch(&pr(3), "s-1", forever(), now() - TimeDelta::days(1));
        watches.record(&[(pr(3), seen_pull(State::Open, None))], now());
        ended(&mut watches, "s-1");

        let back = now() + TimeDelta::minutes(5);
        watches.attach("s-1", address("/tmp/resumed"), back);
        assert_eq!(watches.due_now(back, TimeDelta::zero()), [pr(3)]);

        let events = watches.record(&[(pr(3), seen_pull(State::Approved, None))], back);
        assert_eq!(events.len(), 1, "one transition, not a fresh baseline");
        assert_eq!(
            (events[0].from.as_str(), events[0].to.as_str()),
            ("open", "approved")
        );
        assert_eq!(
            watches.take("s-1", back).len(),
            1,
            "and it is this session's"
        );
    }

    #[test]
    fn a_session_that_ends_after_watching_again_still_pauses() {
        // Found by the hunt: watching cleared the departure without an
        // address to show for it, so the real SessionEnd found nothing to
        // detach and the watch was read, at 2¢ a time, until its deadline.
        let mut watches = watching(State::Open);
        ended(&mut watches, "s-1");
        watches.watch(&pr(3), "s-1", forever(), now());
        watches.detach("s-1");
        assert!(watches.paused(&watches.watches["github.com/dpep/inkling#3"]));
    }

    #[test]
    fn a_channel_watch_is_never_paused() {
        // A deliberate standing interest, not something a session touched:
        // its news keeps accruing for the owner to pull or resume into.
        let channel = Reference::SlackChannel {
            channel: "C0E48AHSN".into(),
            name: Some("incidents".into()),
        };
        let mut watches = Watches::default();
        watches.watch(&channel, "s-1", forever(), now());
        ended(&mut watches, "s-1");
        assert!(!watches.paused(&watches.watches[&channel.key()]));
        assert_eq!(watches.due_now(now(), TimeDelta::zero()), [channel]);
    }

    #[test]
    fn only_attaching_brings_an_ended_session_back() {
        let mut watches = watching(State::Open);
        ended(&mut watches, "s-1");
        watches.watch(&pr(4), "s-1", forever(), now());
        assert!(watches.paused(&watches.watches["github.com/dpep/inkling#4"]));

        watches.attach("s-1", address("/tmp/s"), now());
        assert!(!watches.paused(&watches.watches["github.com/dpep/inkling#3"]));
        assert!(!watches.paused(&watches.watches["github.com/dpep/inkling#4"]));
    }

    #[test]
    fn an_ended_reader_outlives_its_retention_while_it_owns_a_watch() {
        // Forgetting the departure would quietly resume the reads.
        let mut watches = watching(State::Open);
        ended(&mut watches, "s-1");
        watches.take("idle", now());
        let week = now() + SUBSCRIBER_RETENTION + TimeDelta::hours(1);

        watches.collect_garbage(week, RETENTION);
        assert!(watches.knows("s-1"));
        assert!(!watches.knows("idle"));

        watches.unwatch(&pr(3), "s-1");
        watches.collect_garbage(week, RETENTION);
        assert!(!watches.knows("s-1"));
    }

    #[test]
    fn a_reader_that_lost_its_address_is_one_a_hook_attaches_again() {
        // Only a live session fires hooks, so whatever took its address — a
        // probe, SessionEnd, a week of quiet that dropped the record — was
        // wrong or is over.
        let mut watches = watching(State::Open);
        assert!(watches.lost_address("s-1"), "its record holds no address");
        watches.attach("s-1", address("/tmp/s"), now());
        assert!(!watches.lost_address("s-1"));
        watches.detach("s-1");
        assert!(watches.lost_address("s-1"));
        assert!(!watches.lost_address("stranger"), "nobody we know");
        assert!(!watches.lost_address(TERMINAL));
    }

    #[test]
    fn an_unsubscribed_session_stays_unsubscribed_until_it_attaches() {
        // Found by the hunt: the PostToolUse hook of `inkling unsubscribe`
        // itself attached the session again.
        let mut watches = watching(State::Open);
        watches.attach("s-1", address("/tmp/s"), now());
        assert!(watches.unsubscribe("s-1"));
        assert!(!watches.lost_address("s-1"));
        assert!(
            watches.paused(&watches.watches[&pr(3).key()]),
            "and its watches pause"
        );

        watches.attach("s-1", address("/tmp/s"), now());
        watches.detach("s-1");
        assert!(watches.lost_address("s-1"), "attaching ends the choice");
    }

    #[test]
    fn a_reader_quiet_for_a_week_keeps_what_it_is_still_owed() {
        // Found by the hunt: a `--ttl 30d` watch, nobody asking for eight
        // days, then a transition an hour old was dropped with its reader.
        let mut watches = Watches::default();
        watches.watch(&pr(3), "s-1", now() + TimeDelta::days(30), now());
        watches.watch(&pr(4), "s-1", now() + TimeDelta::days(30), now());
        watches.record(&[(pr(3), seen_pull(State::Open, None))], now());
        watches.record(&[(pr(4), seen_pull(State::Open, None))], now());
        let week = now() + SUBSCRIBER_RETENTION + TimeDelta::hours(1);
        watches.record(&[(pr(3), seen_pull(State::Approved, None))], week);
        // #4 finishing removes its watch; its merge is still owed.
        watches.record(&[(pr(4), seen_pull(State::Merged, None))], week);

        watches.collect_garbage(week, RETENTION);
        watches.unwatch(&pr(3), "s-1");
        watches.collect_garbage(week, RETENTION);
        assert_eq!(watches.take("s-1", week).len(), 2);
    }

    #[test]
    fn a_reader_with_nothing_left_is_forgotten_after_a_week() {
        let mut watches = watching(State::Open);
        watches.attach("s-1", address("/tmp/s"), now());
        watches.unwatch(&pr(3), "s-1");
        let week = now() + SUBSCRIBER_RETENTION + TimeDelta::hours(1);
        watches.collect_garbage(week, RETENTION);
        assert!(!watches.knows("s-1"), "an address alone doesn't keep it");
    }

    #[test]
    fn only_paused_watches_give_a_server_no_reason_to_stay() {
        let mut watches = watching(State::Open);
        watches.attach("s-1", address("/tmp/s"), now());
        watches.attach("s-2", address("/tmp/t"), now());
        assert!(crate::push::should_serve(&watches, now()));

        watches.detach("s-1");
        assert!(
            !crate::push::should_serve(&watches, now()),
            "s-2 watches nothing"
        );
        assert!(watches.reachable("s-1", now()), "but status still shows it");
    }

    #[test]
    fn a_hook_asks_whether_to_attach_without_parsing_a_store_that_names_its_socket() {
        let store = WatchStore::new(scratch("lost-address"));
        let socket = Path::new("/tmp/s");
        assert!(
            !store.lost_address("s-1", socket),
            "no store, nobody to attach"
        );
        store
            .update(now(), |w| {
                w.watch(&pr(3), "s-1", forever(), now());
            })
            .unwrap();
        assert!(store.lost_address("s-1", socket));

        // Parsed, s-1 has no address; but the socket is named, so the answer
        // comes from the text alone.
        store
            .update(now(), |w| {
                w.attach("s-2", address("/tmp/s"), now());
            })
            .unwrap();
        assert!(store.read().lost_address("s-1"));
        assert!(!store.lost_address("s-1", socket));
    }

    #[test]
    fn a_watch_is_gone_only_after_repeated_misses() {
        let mut watches = watching(State::Open);
        for _ in 1..GONE_AFTER {
            assert!(watches.gone(&[pr(3)], now()).is_empty());
        }
        assert!(
            watches.watches.contains_key(&pr(3).key()),
            "a miss is a failure"
        );
        let ended = watches.gone(&[pr(3)], now());
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].to, "gone");
        assert!(watches.watches.is_empty());
    }

    #[test]
    fn a_reading_between_misses_starts_the_count_again() {
        let mut watches = watching(State::Open);
        for _ in 1..GONE_AFTER {
            watches.gone(&[pr(3)], now());
        }
        watches.record(&[(pr(3), seen_pull(State::Open, None))], now());
        for _ in 1..GONE_AFTER {
            assert!(watches.gone(&[pr(3)], now()).is_empty());
        }
        assert!(watches.watches.contains_key(&pr(3).key()));
    }

    #[test]
    fn a_version_2_store_has_missed_nothing() {
        let mut old = serde_json::to_value(watching(State::Open)).unwrap();
        old["version"] = 2.into();
        old["watches"][pr(3).key()]
            .as_object_mut()
            .unwrap()
            .remove("misses");
        let raw = old.to_string();
        let Ok(watches) = parse(&raw) else {
            panic!("a version 2 store parses")
        };
        assert_eq!(watches.watches[&pr(3).key()].misses, 0);
    }

    #[test]
    fn a_channel_watched_under_both_keys_merges_when_rekeyed() {
        // A store from before channels were keyed by id can hold one watch
        // by name and another by id; rekeying must not drop either's readers.
        let channel = |name: Option<&str>| Reference::SlackChannel {
            channel: "C0E48AHSN".into(),
            name: name.map(Into::into),
        };
        let mut by_name = Watches::default();
        by_name.watch(&channel(Some("general")), "b", forever(), now());
        let posted = Receipt::Posted {
            channel: "C0E48AHSN".into(),
            ts: "1.1".into(),
        };
        by_name.receipt("b", &posted, now());
        let mut by_id = Watches::default();
        by_id.watch(&channel(None), "a", now() + TimeDelta::days(1), now());
        let mut old = Watches {
            version: 2,
            ..Watches::default()
        };
        old.watches.insert(
            "slack:#general".into(),
            by_name.watches.into_values().next().unwrap(),
        );
        old.watches.insert(
            "slack:C0E48AHSN".into(),
            by_id.watches.into_values().next().unwrap(),
        );

        let raw = serde_json::to_string(&old).unwrap();
        let Ok(watches) = parse(&raw) else {
            panic!("parses")
        };
        assert_eq!(watches.watches.len(), 1);
        let watch = &watches.watches["slack:C0E48AHSN"];
        assert_eq!(watch.reference, channel(Some("general")));
        assert_eq!(watch.owners, ["a".to_string(), "b".to_string()].into());
        assert_eq!(watch.deadline, forever());
        assert!(watch.receipts["b"].posted.contains("1.1"));
    }

    #[test]
    fn a_pause_survives_the_file() {
        let store = WatchStore::new(scratch("paused"));
        store
            .update(now(), |w| {
                w.watch(&pr(3), "s-1", forever(), now());
                ended(w, "s-1");
            })
            .unwrap();
        assert!(store.read().due_now(now(), TimeDelta::zero()).is_empty());
    }
}
