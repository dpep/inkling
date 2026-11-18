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
use crate::adapters::watches_json::{Seen, Watch, WatchStore, Watches};
use crate::domain::model::Reference;
use crate::domain::slack::ts_key;
use crate::domain::watch::{Actors, Event, Sighting, Standing};
use crate::ports::{Clock, IssueProvider, MessageProvider, RefError, StatusProvider, StoreError};
use chrono::{DateTime, TimeDelta, Utc};

pub struct Poller<'a> {
    pub store: &'a WatchStore,
    pub github: &'a dyn StatusProvider,
    pub jira: &'a dyn IssueProvider,
    /// `None` leaves Slack messages unread by this caller. A read is a
    /// `claude -p` call of seconds to a minute, which a prompt hook's budget
    /// can't hold; the resident server and the explicit commands read them.
    pub slack: Option<&'a dyn MessageProvider>,
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
    /// References read as not there at all (`RefError::NotFound`).
    pub missing: Vec<Reference>,
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

/// Read through the bridge: slow and paid for, so only some callers do.
fn is_slack(reference: &Reference) -> bool {
    matches!(
        reference,
        Reference::SlackMessage { .. } | Reference::SlackChannel { .. }
    )
}

/// Whether anything a caller without the bridge would read is due: what the
/// prompt hook asks before paying for a GitHub token.
pub fn due_without_bridge(watches: &Watches, now: DateTime<Utc>, floor: TimeDelta) -> bool {
    watches
        .due_now(now, floor)
        .iter()
        .any(|reference| !is_slack(reference))
}

fn name(so_far: &[String], reference: &Reference, error: &impl std::fmt::Display) {
    match so_far.len() < NAMED_FAILURES {
        true => tracing::warn!(%reference, %error, "could not read watch"),
        false => tracing::debug!(%reference, %error, "could not read watch"),
    }
}

/// A reference that couldn't be read. One that isn't there at all — deleted, or
/// never visible to these credentials — will never be, so its watch is over.
fn unreadable(
    failures: &mut Vec<String>,
    gone: &mut Vec<Reference>,
    reference: &Reference,
    error: RefError,
) {
    name(failures, reference, &error);
    if error == RefError::NotFound {
        gone.push(reference.clone());
    }
    failures.push(format!("{reference}: {error}"));
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
        self.poll_then(floor, why, &mut || {})
    }

    /// `poll`, a source at a time: GitHub, then Jira, then each Slack message
    /// or channel, each recorded as it lands. `between` runs before every
    /// read after the first, so the server can push what is recorded rather
    /// than hold it behind an `acli` or a `claude -p` of a minute or more.
    pub fn poll_then(
        &self,
        floor: TimeDelta,
        why: &str,
        between: &mut dyn FnMut(),
    ) -> Result<Polled, StoreError> {
        self.read_due(floor, why, &|_| true, None, &mut || {
            between();
            true
        })
    }

    /// `poll`, of `these` only: what `inkling watch` just registered, without
    /// paying for anything else that happens to be due.
    pub fn poll_these(&self, these: &[Reference], why: &str) -> Result<Polled, StoreError> {
        let given = |r: &Reference| these.contains(r);
        self.read_due(TimeDelta::zero(), why, &given, None, &mut || true)
    }

    /// The server's Slack worker: each due Slack watch in turn, until `stop`.
    /// Takes no poll lock — the claim is what keeps two readers from paying
    /// for one reading — so GitHub and Jira never wait on the bridge.
    pub fn poll_slack(&self, why: &str, stop: &dyn Fn() -> bool) -> Result<Polled, StoreError> {
        self.read_due(TimeDelta::zero(), why, &is_slack, None, &mut || !stop())
    }

    /// What `which` picks of what is due, a source at a time. `lock` is let go
    /// before the first Slack read; `go_on` runs between reads and can end
    /// the poll.
    fn read_due(
        &self,
        floor: TimeDelta,
        why: &str,
        which: &dyn Fn(&Reference) -> bool,
        lock: Option<FileLock>,
        go_on: &mut dyn FnMut() -> bool,
    ) -> Result<Polled, StoreError> {
        let observed_at = self.clock.now();
        let mut references = self.wanted(&self.store.read(), observed_at, floor);
        references.retain(|r| which(r));
        if references.is_empty() {
            tracing::debug!(why, "nothing is due");
            return Ok(Polled {
                skipped: true,
                ..Polled::default()
            });
        }

        let count = |is: fn(&Reference) -> bool| references.iter().filter(|r| is(r)).count();
        let pulls = count(|r| matches!(r, Reference::GitHubPullRequest { .. }));
        let messages = count(is_slack);
        let mut polled = Polled {
            watched: references.len(),
            ..Polled::default()
        };
        let (slow, quick): (Vec<Reference>, Vec<Reference>) =
            references.iter().cloned().partition(is_slack);
        let (github, jira): (Vec<Reference>, Vec<Reference>) = quick
            .into_iter()
            .partition(|r| matches!(r, Reference::GitHubPullRequest { .. }));
        let mut first = true;
        let mut go_on = || std::mem::take(&mut first) || go_on();
        for group in [github, jira].into_iter().filter(|g| !g.is_empty()) {
            if !go_on() {
                break;
            }
            self.read_and_record(&group, &mut polled)?;
        }
        drop(lock);
        for reference in slow {
            if !go_on() {
                break;
            }
            if self.claim(&reference, floor)? {
                self.read_and_record(std::slice::from_ref(&reference), &mut polled)?;
            }
        }
        let elapsed = (self.clock.now() - observed_at).num_milliseconds();

        tracing::info!(
            why,
            watched = references.len(),
            // Split by provider because they cost differently: pull requests
            // are one batched query, issues one `acli` apiece.
            github = pulls,
            jira = references.len() - pulls - messages,
            slack = messages,
            read = polled.sightings.len(),
            events = polled.events.len(),
            failures = polled.failures.len(),
            elapsed_ms = elapsed,
            "polled watches"
        );
        Ok(polled)
    }

    /// Mark a paid read as tried before paying for it, if it is still due: a
    /// poller arriving mid-read then waits out the floor instead of reading it
    /// again.
    fn claim(&self, reference: &Reference, floor: TimeDelta) -> Result<bool, StoreError> {
        let now = self.clock.now();
        self.store.update(now, |watches| {
            let due = watches.due_now(now, floor).contains(reference);
            if due {
                watches.tried(std::slice::from_ref(reference), now);
            }
            due
        })
    }

    /// Read `references` and record what came back, stamped with when the
    /// read began: the reading can't be older than that.
    fn read_and_record(
        &self,
        references: &[Reference],
        polled: &mut Polled,
    ) -> Result<(), StoreError> {
        let read_at = self.clock.now();
        let mut gone = Vec::new();
        let read = self.sightings(references, &mut polled.failures, &mut gone);
        polled
            .events
            .extend(self.store.update(self.clock.now(), |watches| {
                watches.tried(references, read_at);
                let mut events = watches.record(&read, read_at);
                events.extend(watches.gone(&gone, read_at));
                events
            })?);
        polled.sightings.extend(read);
        polled.missing.extend(gone);
        Ok(())
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
        self.poll_if_stale_then(floor, why, &mut || {})
    }

    /// `poll_if_stale`, with `poll_then`'s `between`. The lock is let go before
    /// any Slack read: the claim already stops a second paid read, and a
    /// prompt hook mustn't skip its GitHub refresh for a minute-long one.
    pub fn poll_if_stale_then(
        &self,
        floor: TimeDelta,
        why: &str,
        between: &mut dyn FnMut(),
    ) -> Result<Polled, StoreError> {
        let now = self.clock.now();
        if self.wanted(&self.store.read(), now, floor).is_empty() {
            tracing::debug!(why, "readings are fresh");
            return Ok(Polled {
                skipped: true,
                ..Polled::default()
            });
        }

        let lock_path = self.store.poll_lock_path();
        let held =
            FileLock::try_acquire(&lock_path).map_err(|e| StoreError::Write(e.to_string()))?;
        let Some(lock) = held else {
            tracing::debug!(why, "another process is polling; reading what it writes");
            return Ok(Polled {
                skipped: true,
                ..Polled::default()
            });
        };
        self.read_due(floor, why, &|_| true, Some(lock), &mut || {
            between();
            true
        })
    }

    /// What this caller would read now: everything due, less what needs a
    /// bridge it doesn't have.
    fn wanted(&self, watches: &Watches, now: DateTime<Utc>, floor: TimeDelta) -> Vec<Reference> {
        watches
            .due_now(now, floor)
            .into_iter()
            .filter(|r| self.slack.is_some() || !is_slack(r))
            .collect()
    }

    /// Ask each provider about the references it answers for.
    ///
    /// Routing by variant in one place, rather than a registry. Three
    /// providers is where docs said a registry would start paying; it still
    /// doesn't, because each arm differs in what it builds, not just whom it
    /// asks.
    fn sightings(
        &self,
        references: &[Reference],
        failures: &mut Vec<String>,
        gone: &mut Vec<Reference>,
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
                                        comments: None,
                                    },
                                    title: status.title,
                                    failing: status.failing,
                                    head: status.head,
                                    by: status.by,
                                    discussion: status.discussion,
                                },
                            )),
                            Err(error) => {
                                unreadable(failures, gone, &reference, error);
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
                                    by: Default::default(),
                                    discussion: None,
                                },
                            )),
                            Err(error) => {
                                unreadable(failures, gone, &reference, error);
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

        let messages: Vec<Reference> = references
            .iter()
            .filter(|r| matches!(r, Reference::SlackMessage { .. }))
            .cloned()
            .collect();
        if let Some(slack) = self.slack.filter(|_| !messages.is_empty()) {
            let watched: Vec<Reference> = self
                .store
                .read()
                .active(self.clock.now())
                .into_iter()
                .filter(|r| matches!(r, Reference::SlackMessage { .. }))
                .collect();
            match slack.read(&messages, &watched) {
                Ok(lookups) => {
                    for (reference, status) in lookups {
                        match status {
                            Ok(status) => seen.push((
                                reference,
                                Sighting {
                                    standing: Standing::Message {
                                        reactions: status.reactions,
                                        replies: status.replies,
                                    },
                                    title: String::new(),
                                    failing: None,
                                    head: None,
                                    by: Actors {
                                        names: status.names,
                                        ..Actors::default()
                                    },
                                    discussion: None,
                                },
                            )),
                            Err(error) => {
                                unreadable(failures, gone, &reference, error);
                            }
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, messages = messages.len(), "slack unreachable");
                    failures.push(error.to_string());
                }
            }
        }

        if let Some(slack) = self.slack {
            self.channels(slack, references, &mut seen, failures, gone);
        }
        seen
    }

    /// Each channel from the newest message its last reading saw. No reading
    /// yet is a baseline, which needs only the newest; a channel that was
    /// empty reads from when the watch began.
    fn channels(
        &self,
        slack: &dyn MessageProvider,
        references: &[Reference],
        seen: &mut Vec<(Reference, Sighting)>,
        failures: &mut Vec<String>,
        gone: &mut Vec<Reference>,
    ) {
        let watches = self.store.read();
        for reference in references {
            let Reference::SlackChannel { channel, .. } = reference else {
                continue;
            };
            let watch = watches.watches.get(&reference.key());
            let before = match watch {
                Some(Watch {
                    seen:
                        Some(Seen {
                            standing: Standing::Channel { newest, .. },
                            ..
                        }),
                    ..
                }) => Some(newest.clone()),
                _ => None,
            };
            // A channel that read empty is read from when the watch began: a
            // first read that wrongly came back empty must not make its
            // history news.
            let began = watch.map(|w| format!("{}.000000", w.since.timestamp()));
            let oldest = before
                .as_ref()
                .map(|newest| newest.as_deref().or(began.as_deref()).unwrap_or("0"));
            match slack.channel(channel, oldest) {
                Ok(status) => {
                    let newest = status
                        .arrived
                        .keys()
                        .max_by_key(|ts| ts_key(ts))
                        .cloned()
                        .or_else(|| before.clone().flatten());
                    let baseline = oldest.is_none();
                    seen.push((
                        reference.clone(),
                        Sighting {
                            discussion: None,
                            standing: Standing::Channel {
                                count: 0,
                                newest,
                                arrived: if baseline {
                                    Default::default()
                                } else {
                                    status.arrived
                                },
                                more: status.more && !baseline,
                            },
                            title: String::new(),
                            failing: None,
                            head: None,
                            by: Actors {
                                names: status.names,
                                ..Actors::default()
                            },
                        },
                    ));
                }
                Err(error) => {
                    unreadable(failures, gone, reference, error);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::watches_json::{FASTEST_READ, RETENTION};
    use crate::domain::model::State;
    use crate::domain::wait::CheckState;
    use crate::domain::watch::{Axis, Category};
    use crate::ports::{
        ChannelStatus, IssueLookup, IssueStatus, MessageLookup, MessageStatus, ProviderError,
        RefError,
    };
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

    #[derive(Default)]
    struct ScriptedMessages {
        statuses: RefCell<HashMap<String, MessageStatus>>,
        calls: std::cell::Cell<usize>,
        /// A channel's messages, ts -> author, and each `oldest` asked for.
        posts: RefCell<std::collections::BTreeMap<String, String>>,
        asked: RefCell<Vec<Option<String>>>,
        /// What the last read was told is watched.
        watched: RefCell<Vec<Reference>>,
    }

    impl ScriptedMessages {
        fn replied(&self, reference: &Reference, replies: &[(&str, &str)]) {
            self.statuses.borrow_mut().insert(
                reference.to_string(),
                MessageStatus {
                    replies: replies
                        .iter()
                        .map(|(ts, who)| (ts.to_string(), who.to_string()))
                        .collect(),
                    names: [("U1".to_string(), "alice".to_string())].into(),
                    ..MessageStatus::default()
                },
            );
        }
    }

    impl MessageProvider for ScriptedMessages {
        fn read(
            &self,
            messages: &[Reference],
            watched: &[Reference],
        ) -> Result<Vec<MessageLookup>, ProviderError> {
            self.watched.replace(watched.to_vec());
            self.calls.set(self.calls.get() + messages.len());
            let statuses = self.statuses.borrow();
            Ok(messages
                .iter()
                .map(|r| {
                    let status = statuses.get(&r.to_string()).cloned();
                    (
                        r.clone(),
                        status.ok_or(RefError::Other("bridge failed".into())),
                    )
                })
                .collect())
        }

        fn channel(&self, _: &str, oldest: Option<&str>) -> Result<ChannelStatus, RefError> {
            self.asked.borrow_mut().push(oldest.map(Into::into));
            let posts = self.posts.borrow();
            let arrived = match oldest {
                Some(oldest) => posts
                    .iter()
                    .filter(|(ts, _)| crate::domain::slack::later(ts, oldest) || oldest == "0")
                    .map(|(ts, who)| (ts.clone(), who.clone()))
                    .collect(),
                None => posts
                    .iter()
                    .next_back()
                    .map(|(ts, who)| (ts.clone(), who.clone()))
                    .into_iter()
                    .collect(),
            };
            Ok(ChannelStatus {
                arrived,
                names: [("U1".to_string(), "alice".to_string())].into(),
                ..ChannelStatus::default()
            })
        }
    }

    fn slack() -> Reference {
        Reference::SlackMessage {
            channel: "C0123ABCD".into(),
            ts: "1786596050.245879".into(),
            thread: None,
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
        slack: ScriptedMessages,
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
                slack: ScriptedMessages::default(),
                clock: FixedClock::at("2026-09-20T12:00:00Z"),
            }
        }

        fn poller(&self) -> Poller<'_> {
            Poller {
                store: &self.store,
                github: &self.github,
                jira: &self.jira,
                slack: Some(&self.slack),
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
    fn a_watch_whose_reference_is_gone_says_so_once_and_finishes() {
        // A deleted message or a channel the connector can't see would
        // otherwise be paid for every five minutes until its deadline.
        let h = Harness::new("gone");
        h.github.fail(&pr(3), RefError::NotFound);
        h.watch(&pr(3));
        for _ in 1..crate::adapters::watches_json::GONE_AFTER {
            assert!(h.poll().events.is_empty(), "one miss can be wrong");
        }
        let polled = h.poll();
        assert_eq!(polled.failures.len(), 1);
        assert_eq!(labels(&polled.events), [("state", "watched", "gone")]);
        assert!(h.store.read().watches.is_empty());
        h.clock
            .advance(crate::adapters::watches_json::SLACK_READ_FLOOR);
        assert!(h.poll().events.is_empty(), "once");
    }

    fn labels(events: &[Event]) -> Vec<(&str, &str, &str)> {
        events
            .iter()
            .map(|e| (e.axis.as_str(), e.from.as_str(), e.to.as_str()))
            .collect()
    }

    #[test]
    fn a_slack_reply_from_someone_else_is_news_and_names_them() {
        let h = Harness::new("slack");
        h.slack.replied(&slack(), &[]);
        h.watch(&slack());
        assert!(h.poll().events.is_empty(), "baseline");

        h.clock
            .advance(crate::adapters::watches_json::SLACK_READ_FLOOR);
        h.slack.replied(&slack(), &[("1786596060.000001", "U1")]);
        let polled = h.poll();
        assert_eq!(polled.events.len(), 1);
        assert_eq!(polled.events[0].axis, Axis::Replies);
        assert_eq!(polled.events[0].detail.as_deref(), Some("@alice"));
        assert!(polled.events[0].receipt_for.is_empty());
    }

    #[test]
    fn a_channel_is_read_from_the_newest_message_its_last_reading_saw() {
        let h = Harness::new("channel");
        let general = Reference::SlackChannel {
            channel: "C0E48AHSN".into(),
            name: Some("general".into()),
        };
        h.slack
            .posts
            .borrow_mut()
            .insert("1786596000.000001".into(), "U9".into());
        h.watch(&general);
        assert!(h.poll().events.is_empty(), "baseline");

        h.clock
            .advance(crate::adapters::watches_json::SLACK_READ_FLOOR);
        assert!(h.poll().events.is_empty(), "nothing new");

        h.clock
            .advance(crate::adapters::watches_json::SLACK_READ_FLOOR);
        h.slack.posts.borrow_mut().extend([
            ("1786596100.000001".to_string(), "U1".to_string()),
            ("1786596200.000001".to_string(), "U1".to_string()),
        ]);
        let polled = h.poll();
        assert_eq!(polled.events.len(), 1);
        assert_eq!(polled.events[0].axis, Axis::Messages);
        assert_eq!(
            (polled.events[0].from.as_str(), polled.events[0].to.as_str()),
            ("0", "2")
        );
        assert_eq!(polled.events[0].detail.as_deref(), Some("@alice"));
        assert_eq!(
            *h.slack.asked.borrow(),
            [
                None,
                Some("1786596000.000001".into()),
                Some("1786596000.000001".into())
            ],
            "a baseline asks for the newest, then each read starts where the last ended"
        );
    }

    #[test]
    fn a_channel_that_read_empty_counts_only_what_came_after_the_watch() {
        // A flaky empty first read must not turn a channel's history into news.
        let h = Harness::new("empty-first");
        let general = Reference::SlackChannel {
            channel: "C0E48AHSN".into(),
            name: None,
        };
        h.watch(&general);
        assert!(h.poll().events.is_empty(), "baseline, read as empty");

        h.clock
            .advance(crate::adapters::watches_json::SLACK_READ_FLOOR);
        h.slack.posts.borrow_mut().extend([
            ("1786596000.000001".to_string(), "U9".to_string()),
            ("1789905900.000001".to_string(), "U1".to_string()),
        ]);
        let polled = h.poll();
        assert_eq!(polled.events.len(), 1);
        assert_eq!(polled.events[0].to, "1", "{:?}", polled.events);
        assert_eq!(
            h.slack.asked.borrow()[1].as_deref(),
            Some("1789905600.000000"),
            "from when the watch began"
        );
    }

    #[test]
    fn a_caller_without_the_bridge_leaves_slack_unread() {
        // The prompt hook: a read of seconds to a minute can't fit its budget.
        let h = Harness::new("no-bridge");
        h.watch(&slack());
        let poller = Poller {
            slack: None,
            ..h.poller()
        };
        assert!(poller.poll(TimeDelta::zero(), "test").unwrap().skipped);
        assert_eq!(h.slack.calls.get(), 0);
    }

    #[test]
    fn a_failing_slack_read_is_not_retried_every_tick() {
        // Each attempt is a paid `claude -p` call, success or not.
        let h = Harness::new("slack-failing");
        h.watch(&slack());
        let polled = h.poll();
        assert_eq!(polled.failures.len(), 1);
        h.poll();
        assert_eq!(h.slack.calls.get(), 1, "a minute later is inside the floor");
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
        h.github
            .fail(&pr(3), RefError::Other("rate limited".into()));

        h.poll();
        assert!(
            h.store.read().watches["github.com/dpep/inkling#3"]
                .seen
                .is_none(),
            "a reference that couldn't be read has no baseline to record"
        );
    }

    /// Every session is listening, and what it was sent is kept.
    #[derive(Default)]
    struct Listening {
        sent: RefCell<Vec<String>>,
    }

    impl crate::push::Sessions for Listening {
        fn deliver(
            &self,
            _: &crate::adapters::session_socket::SessionAddress,
            text: &str,
        ) -> Result<
            crate::adapters::session_socket::Written,
            crate::adapters::session_socket::DeliverError,
        > {
            self.sent.borrow_mut().push(text.to_string());
            Ok(crate::adapters::session_socket::Written)
        }

        fn probe(
            &self,
            _: &crate::adapters::session_socket::SessionAddress,
        ) -> Result<(), crate::adapters::session_socket::DeliverError> {
            Ok(())
        }
    }

    /// The scripted bridge, with a look at the world at the moment a paid
    /// read starts.
    struct Spy<'a> {
        inner: &'a ScriptedMessages,
        on_read: Box<dyn Fn() + 'a>,
    }

    impl MessageProvider for Spy<'_> {
        fn read(
            &self,
            messages: &[Reference],
            watched: &[Reference],
        ) -> Result<Vec<MessageLookup>, ProviderError> {
            (self.on_read)();
            self.inner.read(messages, watched)
        }
        fn channel(&self, channel: &str, oldest: Option<&str>) -> Result<ChannelStatus, RefError> {
            (self.on_read)();
            self.inner.channel(channel, oldest)
        }
    }

    #[test]
    fn a_slack_read_is_told_every_live_watched_message() {
        // Not just the due ones: a reply belongs to the latest watched
        // message before it, whether or not that one is read this time.
        let h = Harness::new("slack-watched");
        let later = Reference::SlackMessage {
            channel: "C0123ABCD".into(),
            ts: "1786596090.000000".into(),
            thread: None,
        };
        let channel = Reference::SlackChannel {
            channel: "C0123ABCD".into(),
            name: None,
        };
        h.slack.replied(&slack(), &[]);
        h.watch(&slack());
        h.watch(&later);
        h.watch(&channel);
        h.watch(&pr(3));
        h.github.set(&pr(3), State::Open);
        h.poller()
            .poll_these(std::slice::from_ref(&slack()), "test")
            .unwrap();
        let mut told = h.slack.watched.borrow().clone();
        told.sort();
        let mut want = vec![slack(), later];
        want.sort();
        assert_eq!(told, want);
    }

    #[test]
    fn github_news_is_recorded_and_pushed_before_the_slack_reads_begin() {
        // Found by the hunt: a red PR sat in memory for a minute or more
        // while the same poll waited on `claude -p` for a Slack message.
        use crate::serve::Work;
        let h = Harness::new("quick-first");
        h.github.set(&pr(3), State::Open);
        h.slack.replied(&slack(), &[]);
        h.watch(&pr(3));
        h.watch(&slack());
        h.poll();
        h.store
            .update(h.clock.now(), |w| {
                w.attach(
                    "test",
                    crate::adapters::session_socket::SessionAddress::new("/tmp/t", None),
                    h.clock.now(),
                )
            })
            .unwrap();
        h.clock
            .advance(crate::adapters::watches_json::SLACK_READ_FLOOR);
        h.github.set(&pr(3), State::Merged);

        let sessions = Listening::default();
        let pushed_before_read = std::cell::Cell::new(None);
        let spy = Spy {
            inner: &h.slack,
            on_read: Box::new(|| pushed_before_read.set(Some(sessions.sent.borrow().len()))),
        };
        let tick = crate::push::Tick {
            poller: Poller {
                slack: Some(&spy),
                ..h.poller()
            },
            sessions: &sessions,
            clock: &h.clock,
        };
        tick.tick().unwrap();

        assert_eq!(pushed_before_read.get(), Some(1), "merged was pushed first");
    }

    #[test]
    fn a_slack_read_is_claimed_before_it_is_paid_for() {
        // Found by the hunt: `tried_at` was stamped after the read, so a
        // second poller during a minute-long read paid for it again.
        let h = Harness::new("claimed");
        h.slack.replied(&slack(), &[]);
        h.watch(&slack());
        let claimed = std::cell::Cell::new(false);
        let spy = Spy {
            inner: &h.slack,
            on_read: Box::new(|| {
                claimed.set(
                    h.store
                        .read()
                        .due_now(h.clock.now(), TimeDelta::zero())
                        .is_empty(),
                )
            }),
        };
        Poller {
            slack: Some(&spy),
            ..h.poller()
        }
        .poll(TimeDelta::zero(), "test")
        .unwrap();
        assert!(claimed.get(), "another poller would have read it too");
    }

    fn another_slack() -> Reference {
        Reference::SlackMessage {
            channel: "C0123ABCD".into(),
            ts: "1786596060.245879".into(),
            thread: None,
        }
    }

    #[test]
    fn each_slack_reading_is_recorded_as_it_lands_and_stamped_when_it_was_read() {
        // Found by the hunt: every reading waited for the last read of the
        // poll, and was stamped with the time the first one began.
        let h = Harness::new("per-read");
        h.slack.replied(&slack(), &[]);
        h.slack.replied(&another_slack(), &[]);
        h.watch(&slack());
        h.watch(&another_slack());
        h.poll();
        h.clock
            .advance(crate::adapters::watches_json::SLACK_READ_FLOOR);
        h.slack.replied(&slack(), &[("1786596100.000001", "U1")]);
        h.slack
            .replied(&another_slack(), &[("1786596100.000002", "U1")]);

        let recorded_before_next = RefCell::new(Vec::new());
        let spy = Spy {
            inner: &h.slack,
            on_read: Box::new(|| {
                let events = h.store.read().events.len();
                recorded_before_next.borrow_mut().push(events);
                // Each bridge call takes a while.
                h.clock.advance(TimeDelta::seconds(30));
            }),
        };
        let started = h.clock.now();
        let polled = Poller {
            slack: Some(&spy),
            ..h.poller()
        }
        .poll(TimeDelta::zero(), "test")
        .unwrap();

        assert_eq!(*recorded_before_next.borrow(), vec![0, 1]);
        let at: Vec<_> = polled.events.iter().map(|e| e.at).collect();
        assert_eq!(at, vec![started, started + TimeDelta::seconds(30)]);
    }

    #[test]
    fn the_poll_lock_is_let_go_before_a_slack_read() {
        // A prompt hook found the lock held for a whole Slack phase and
        // skipped the GitHub refresh it had come for.
        let h = Harness::new("lock-released");
        h.slack.replied(&slack(), &[]);
        h.watch(&slack());
        let free = std::cell::Cell::new(None);
        let spy = Spy {
            inner: &h.slack,
            on_read: Box::new(|| {
                let lock = FileLock::try_acquire(&h.store.poll_lock_path()).unwrap();
                free.set(Some(lock.is_some()));
            }),
        };
        Poller {
            slack: Some(&spy),
            ..h.poller()
        }
        .poll_if_stale(TimeDelta::zero(), "test")
        .unwrap();
        assert_eq!(free.get(), Some(true));
    }

    #[test]
    fn the_slack_worker_reads_only_slack_and_stops_between_reads() {
        let h = Harness::new("slack-worker");
        h.github.set(&pr(3), State::Open);
        h.slack.replied(&slack(), &[]);
        h.slack.replied(&another_slack(), &[]);
        h.watch(&pr(3));
        h.watch(&slack());
        h.watch(&another_slack());

        let stop = std::cell::Cell::new(false);
        let spy = Spy {
            inner: &h.slack,
            on_read: Box::new(|| stop.set(true)),
        };
        let polled = Poller {
            slack: Some(&spy),
            ..h.poller()
        }
        .poll_slack("test", &|| stop.get())
        .unwrap();

        assert_eq!(polled.sightings.len(), 1, "stopped after the first read");
        assert_eq!(h.github.calls(), 0, "GitHub is the tick's");
        let due = h.store.read().due_now(h.clock.now(), TimeDelta::zero());
        assert_eq!(due.len(), 2, "the PR and the unread message: {due:?}");
    }

    #[test]
    fn reading_what_was_just_watched_reads_nothing_else() {
        // `inkling watch <pr>` waited on a paid Slack read it hadn't asked for.
        let h = Harness::new("these");
        h.github.set(&pr(3), State::Open);
        h.github.set(&pr(4), State::Open);
        h.slack.replied(&slack(), &[]);
        h.watch(&pr(3));
        h.watch(&pr(4));
        h.watch(&slack());

        let polled = h.poller().poll_these(&[pr(3)], "test").unwrap();

        let read: Vec<_> = polled.sightings.iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(read, vec![pr(3)]);
        assert_eq!(h.slack.calls.get(), 0);
    }

    /// Issues that look at the store when asked, the way `Spy` does.
    struct IssueSpy<'a> {
        inner: &'a ScriptedIssues,
        on_read: Box<dyn Fn() + 'a>,
    }

    impl IssueProvider for IssueSpy<'_> {
        fn read(&self, keys: &[String]) -> Result<Vec<IssueLookup>, ProviderError> {
            (self.on_read)();
            self.inner.read(keys)
        }
    }

    #[test]
    fn github_is_recorded_before_jira_is_read() {
        // A slow `acli` held a red PR's reading back with it.
        let h = Harness::new("github-first");
        h.github.set(&pr(3), State::Open);
        h.jira.set("AI-1", "To Do", Category::New);
        h.watch(&pr(3));
        h.watch(&issue("AI-1"));
        h.poll();
        h.github.set(&pr(3), State::Merged);

        let recorded = std::cell::Cell::new(0);
        let jira = IssueSpy {
            inner: &h.jira,
            on_read: Box::new(|| recorded.set(h.store.read().events.len())),
        };
        Poller {
            jira: &jira,
            ..h.poller()
        }
        .poll(TimeDelta::zero(), "test")
        .unwrap();
        assert_eq!(recorded.get(), 1, "merged was recorded before acli ran");
    }

    #[test]
    fn only_slack_due_is_nothing_due_for_a_caller_without_the_bridge() {
        // The prompt hook resolved a GitHub token to read nothing.
        let h = Harness::new("bridge-only");
        h.watch(&slack());
        let watches = h.store.read();
        assert!(!due_without_bridge(
            &watches,
            h.clock.now(),
            TimeDelta::zero()
        ));
        h.watch(&pr(3));
        let watches = h.store.read();
        assert!(due_without_bridge(
            &watches,
            h.clock.now(),
            TimeDelta::zero()
        ));
    }

    #[test]
    fn a_server_that_meets_a_newer_store_stands_down_for_it() {
        use crate::serve::{Pulse, Work};
        let h = Harness::new("superseded");
        let newer = r#"{"version": 999, "watches": {"x": {"reference": {"kind": "pigeon"}}}}"#;
        std::fs::write(h.store.path(), newer).unwrap();
        let tick = crate::push::Tick {
            poller: h.poller(),
            sessions: &Listening::default(),
            clock: &h.clock,
        };

        assert_eq!(tick.tick().unwrap().pulse, Pulse::Superseded);
        assert_eq!(std::fs::read_to_string(h.store.path()).unwrap(), newer);
    }
}
