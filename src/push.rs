//! Telling a running session what moved, instead of waiting to be asked.
//!
//! Pull already works: `inkling events --subscriber <id>` hands a session each
//! transition once. Its two limits are the reason this exists — readings can
//! be a refresh window old when the prompt arrives, and a session that is
//! waiting rather than working never runs a prompt hook at all.
//!
//! The model does not change to accommodate that. A subscriber is a named
//! cursor that may also carry an address, and push and pull drain the same
//! cursor:
//!
//! > Each transition is delivered to each subscriber that was watching it
//! > once, by whichever of push and pull reaches it first.
//!
//! What that costs is stated rather than hidden. The session socket sends no
//! ack, so a write that succeeds means the bytes reached a live socket and
//! nothing more. Advancing on a successful write therefore loses a message
//! that the session held or refused — `events --since <seq>` is where it is
//! still recoverable. The alternative, advancing only on a pull, repeats every
//! pushed transition at the next prompt, which spends the feed's one promise
//! on a case that only happens in some session configurations. See
//! docs/watching.md.
//!
//! Inkling still only observes (DESIGN §1). It delivers news into a session;
//! what the session does about it is the session's business.

use crate::adapters::session_socket::{DeliverError, SessionAddress, Written};
use crate::adapters::watches_json::Watches;
use crate::domain::model::{Reference, State};
use crate::domain::wait::CheckState;
use crate::domain::watch::{Axis, Event, addressed};
use crate::ports::Clock;
use crate::serve::{Beat, Pulse, Ticked, Work};
use crate::watch::Poller;
use chrono::{DateTime, TimeDelta, Utc};

/// What one transition says when it arrives in a session.
///
/// Built only from fields inkling controls — the reference, the axis, the two
/// standings, the name of the check that went red. Deliberately not the pull
/// request's title or the issue's summary: this lands inside an agent's turn,
/// and prose somebody else wrote does not belong there.
pub fn message(event: &Event) -> String {
    digest(&[event])
}

/// Everything one round owes a session, as one message: a merge queue that
/// lands fifteen pull requests costs one turn, not fifteen.
fn digest(events: &[&Event]) -> String {
    match lines(events).as_slice() {
        [line] => format!("inkling: {line}"),
        lines => {
            let mut text = format!("inkling: {} changes", lines.len());
            for line in lines {
                text.push_str(&format!("\n- {line}"));
            }
            text
        }
    }
}

/// One line per change, with what one reading added to a message's emoji —
/// one event per emoji — said once: `3 new reactions :dart: :+1:`.
fn lines(events: &[&Event]) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut run: Vec<&Event> = Vec::new();
    for event in events {
        let joins = run.last().is_some_and(|last| {
            last.reference == event.reference
                && last.axis == Axis::Reactions
                && event.axis == Axis::Reactions
                && growth(last).is_some()
                && growth(event).is_some()
        });
        if !joins && !run.is_empty() {
            lines.push(line(&run));
            run.clear();
        }
        run.push(event);
    }
    if !run.is_empty() {
        lines.push(line(&run));
    }
    lines
}

/// `run` is one event, or several reactions on one reference that grew.
fn line(run: &[&Event]) -> String {
    let event = run[0];
    let moved = match run {
        [_] => said(event),
        _ => {
            let count = run.iter().filter_map(|e| growth(e)).sum();
            let emoji: Vec<&str> = run.iter().filter_map(|e| e.detail.as_deref()).collect();
            tally(count, REACTION, false, &[], &emoji)
        }
    };
    let mut text = format!("{} | {moved}", event.reference);
    if let Some(command) = more(event) {
        text.push_str(&format!(" ({command})"));
    }
    text
}

/// What moved, as a reader is told it: `open → merged by @x` for an axis
/// that moves between standings, `2 comments from @alice` for one that
/// accumulates.
pub fn said(event: &Event) -> String {
    if let Some(tallied) = tallied(event) {
        return tallied;
    }
    let mut text = format!("{} {} → {}", event.axis, event.from, event.to);
    // A state's detail is who caused it ("by @x"), a check's its name; an
    // accumulating axis only lands here when it shrank.
    match (&event.detail, event.axis) {
        (Some(detail), Axis::Checks | Axis::Status) => text.push_str(&format!(": {detail}")),
        (Some(detail), _) => text.push_str(&format!(" {detail}")),
        (None, _) => {}
    }
    text
}

/// `2 comments from @alice`, when the event is an accumulating axis that
/// grew. A shrink — a deleted Slack reply, a removed reaction — isn't news
/// of more, and reads as `from → to`.
pub fn tallied(event: &Event) -> Option<String> {
    let count = growth(event)?;
    let detail = event.detail.as_deref().unwrap_or_default();
    Some(match event.axis {
        Axis::Reactions => tally(count, REACTION, false, &[], &[detail]),
        axis => {
            let noun = if axis == Axis::Comments {
                COMMENT
            } else {
                REPLY
            };
            // Events stored before this wording carried "by …".
            let names = detail.strip_prefix("by ").unwrap_or(detail);
            let names: Vec<&str> = names.split(", ").filter(|n| !n.is_empty()).collect();
            tally(count, noun, event.partial, &names, &[])
        }
    })
}

/// How much an accumulating axis grew by, or `None` for any other axis and
/// for one that didn't grow.
fn growth(event: &Event) -> Option<u64> {
    if !matches!(event.axis, Axis::Comments | Axis::Replies | Axis::Reactions) {
        return None;
    }
    let (from, to) = (
        event.from.parse::<u64>().ok()?,
        event.to.parse::<u64>().ok()?,
    );
    (to > from).then(|| to - from)
}

/// A thing that accumulates, singular and plural.
pub type Noun = (&'static str, &'static str);
pub const COMMENT: Noun = ("comment", "comments");
pub const REPLY: Noun = ("reply", "replies");
pub const REACTION: Noun = ("reaction", "reactions");

/// How many people a line names; past that it counts them.
const NAMED: usize = 3;

/// The one wording for anything that accumulates: `2 comments from @alice
/// and coderabbitai (bot)`, `1 reaction :dart:`, `at least 40 comments from
/// 7 people`. `partial` when the count is a floor. Every line is news, so
/// there is no "new".
pub fn tally(count: u64, noun: Noun, partial: bool, from: &[&str], tags: &[&str]) -> String {
    let word = if count == 1 { noun.0 } else { noun.1 };
    let at_least = if partial { "at least " } else { "" };
    let mut text = format!("{at_least}{count} {word}");
    match from {
        [] => {}
        [one] => text.push_str(&format!(" from {one}")),
        [rest @ .., last] if from.len() <= NAMED => {
            text.push_str(&format!(" from {} and {last}", rest.join(", ")))
        }
        _ => text.push_str(&format!(" from {} people", from.len())),
    }
    for tag in tags.iter().filter(|t| !t.is_empty()) {
        text.push_str(&format!(" {tag}"));
    }
    text
}

/// The one command that says more, in the cases where there obviously is
/// one: checks went red, and the next question is always why; somebody
/// commented, and the next step is reading it, which the push never carries.
///
/// For an Actions job that is its log, which GitHub serves the moment the job
/// ends. Not `gh run view --log-failed`: it refuses until every job in the run
/// has finished, so it can't answer at the moment this is pushed. Anything
/// else gets the list of checks, which is the most that can be said for it.
fn more(event: &Event) -> Option<String> {
    let Reference::GitHubPullRequest {
        host,
        owner,
        repo,
        number,
    } = &event.reference
    else {
        return None;
    };
    if event.axis == Axis::Comments {
        return Some(format!(
            "gh pr view {number} --repo {host}/{owner}/{repo} --comments"
        ));
    }
    if event.axis != Axis::Checks || event.to != CheckState::Failure.as_str() {
        return None;
    }
    Some(match event.job {
        Some(job) => {
            format!("gh api --hostname {host} repos/{owner}/{repo}/actions/jobs/{job}/logs")
        }
        None => format!("gh pr checks {number} --repo {host}/{owner}/{repo}"),
    })
}

/// How long red must last before it is pushed: one later reading at least
/// this much after it. CI that marks a build red while a retry is still
/// running — Buildkite's "failing" — clears within that, and a reader told
/// of it finds nothing failing by the time they look. The reading schedule
/// re-reads a watch a minute after it moves, so this costs about a minute.
pub const RED_SETTLE: TimeDelta = TimeDelta::seconds(30);

/// What to do with a transition that is otherwise worth a turn.
#[derive(Debug, PartialEq, Eq)]
enum Ripe {
    Push,
    /// Already overtaken: a later reading moved its checks again.
    Pass,
    /// Not yet known to last. It, and everything after it, waits for a round
    /// that can tell — the cursor is one number, so nothing may overtake it.
    Hold,
}

fn ripe(event: &Event, watches: &Watches) -> Ripe {
    if event.axis != Axis::Checks || event.to != CheckState::Failure.as_str() {
        return Ripe::Push;
    }
    let overtaken = watches.events.iter().any(|later| {
        later.seq > event.seq && later.axis == Axis::Checks && later.reference == event.reference
    });
    if overtaken {
        return Ripe::Pass;
    }
    // A watch that has gone will not be read again, so nothing would ever
    // settle it.
    let Some(seen) = watches
        .watches
        .get(&event.reference.to_string())
        .and_then(|watch| watch.seen.as_ref())
    else {
        return Ripe::Push;
    };
    match seen.at - event.at >= RED_SETTLE {
        true => Ripe::Push,
        false => Ripe::Hold,
    }
}

/// Whether a transition is worth starting a turn over.
///
/// A push costs the session a turn; a pull costs nothing. A start — a suite
/// beginning, a draft marked ready — leaves nothing to do, so it waits in the
/// feed for the next prompt, and the verdict that follows passes over it. One
/// rule rather than a setting — see docs/watching.md for what would change
/// that, and for why who caused a transition is not part of it.
fn worth_a_turn(event: &Event) -> bool {
    let started = match event.axis {
        Axis::Checks => event.to == CheckState::Pending.as_str(),
        Axis::State => event.from == State::Draft.as_str() && event.to == State::Open.as_str(),
        Axis::Status | Axis::Replies | Axis::Reactions | Axis::Comments => false,
    };
    // What the reader did themself — a reply or a comment only they wrote —
    // is a receipt.
    !started && !event.own
}

/// Where a session's inbox is reached. A seam rather than an abstraction: it
/// exists so the fan-out below — who is told what, whose cursor moves, whose
/// address is dropped — is decided in a test without binding a socket.
pub trait Sessions {
    fn deliver(&self, address: &SessionAddress, text: &str) -> Result<Written, DeliverError>;
    fn probe(&self, address: &SessionAddress) -> Result<(), DeliverError>;
}

/// The real one.
pub struct RealSessions;

impl Sessions for RealSessions {
    fn deliver(&self, address: &SessionAddress, text: &str) -> Result<Written, DeliverError> {
        address.deliver(text)
    }

    fn probe(&self, address: &SessionAddress) -> Result<(), DeliverError> {
        address.probe()
    }
}

/// What one round decided about one subscriber.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub subscriber: String,
    /// The highest sequence number actually written. `None` when nothing was.
    pub cursor: Option<u64>,
    /// The address is not this session's any more.
    pub detach: bool,
    pub written: usize,
    /// Nothing was owed, so liveness was the only question asked.
    pub probed: bool,
}

impl Outcome {
    fn for_subscriber(subscriber: &str) -> Self {
        Self {
            subscriber: subscriber.to_string(),
            ..Self::default()
        }
    }

    /// Fold into the store. Runs inside the data lock, which is why delivery
    /// itself happens outside it: a wedged session must not hold up a prompt.
    pub fn apply(&self, watches: &mut Watches, now: DateTime<Utc>) {
        if let Some(seq) = self.cursor {
            watches.pushed(&self.subscriber, seq, self.written, now);
        }
        if self.detach {
            watches.detach(&self.subscriber);
        }
    }
}

/// Push everything owed to everyone attached, and ask after the rest.
///
/// Takes no lock and writes nothing: the store is read before and written
/// after, so a session that hangs costs this round and nothing else.
pub fn round(sessions: &dyn Sessions, watches: &Watches, clock: &dyn Clock) -> Vec<Outcome> {
    watches
        .attached()
        .into_iter()
        .map(|(subscriber, address, cursor)| {
            // Addressed, not merely newer: a session hears about the things
            // it is watching and nothing else, however many others are
            // running on this machine.
            let mut owed: Vec<&Event> = Vec::new();
            for event in addressed(&watches.events, cursor, subscriber) {
                if !worth_a_turn(event) {
                    continue;
                }
                match ripe(event, watches) {
                    Ripe::Push => owed.push(event),
                    Ripe::Pass => {}
                    Ripe::Hold => break,
                }
            }
            // A session is only ever told so much. What the budget withholds
            // stays in the feed for the next prompt, so this caps the turns
            // inkling can cost without dropping anything.
            let allowance = watches.allowance(subscriber, clock.now());
            if !owed.is_empty() && allowance == 0 {
                tracing::info!(
                    subscriber,
                    owed = owed.len(),
                    window_minutes = crate::adapters::watches_json::PUSH_WINDOW.num_minutes(),
                    "withheld; the rest waits for a prompt"
                );
            }
            let due: Vec<&Event> = match allowance {
                0 => Vec::new(),
                _ => owed,
            };
            match due.is_empty() {
                true => probe(sessions, subscriber, address),
                false => push(sessions, subscriber, address, &due, clock),
            }
        })
        .collect()
}

fn push(
    sessions: &dyn Sessions,
    subscriber: &str,
    address: &SessionAddress,
    due: &[&Event],
    clock: &dyn Clock,
) -> Outcome {
    let mut outcome = Outcome::for_subscriber(subscriber);
    let last = due.last().map(|event| event.seq);
    let started = clock.now();
    let sent = sessions.deliver(address, &digest(due));
    let elapsed_ms = (clock.now() - started).num_milliseconds();

    match sent {
        Ok(Written) => {
            outcome.cursor = last;
            outcome.written = 1;
            tracing::info!(
                subscriber,
                seq = last,
                events = due.len(),
                outcome = "written",
                elapsed_ms,
                advanced = true,
                "pushed"
            );
        }
        Err(error) => {
            outcome.detach = error.drop_address();
            tracing::info!(
                subscriber,
                seq = last,
                events = due.len(),
                outcome = match outcome.detach {
                    true => "gone",
                    false => "transient",
                },
                elapsed_ms,
                advanced = false,
                %error,
                "pushed"
            );
        }
    }
    outcome
}

fn probe(sessions: &dyn Sessions, subscriber: &str, address: &SessionAddress) -> Outcome {
    let mut outcome = Outcome::for_subscriber(subscriber);
    outcome.probed = true;
    if let Err(error) = sessions.probe(address) {
        outcome.detach = error.drop_address();
        match outcome.detach {
            true => tracing::info!(subscriber, %error, "address dropped"),
            false => tracing::debug!(subscriber, %error, "probe did not land; keeping the address"),
        }
    }
    outcome
}

/// Whether a resident server has any reason to exist: somewhere to push to,
/// and something that could still move.
///
/// The same predicate decides whether to start one and whether the one that is
/// running should stay, so the two can never disagree — and neither answer
/// needs a flag or a timer of its own.
pub fn should_serve(watches: &Watches, now: DateTime<Utc>) -> bool {
    watches
        .attached()
        .into_iter()
        .any(|(subscriber, _, _)| watches.has_live_watch(subscriber, now))
}

/// What inkling's resident server does each time round: look upstream, tell
/// whoever is listening, and say whether it is still needed.
pub struct Tick<'a> {
    pub poller: Poller<'a>,
    pub sessions: &'a dyn Sessions,
    pub clock: &'a dyn Clock,
}

impl Work for Tick<'_> {
    fn tick(&self) -> Ticked {
        let started = self.clock.now();
        // The schedule alone decides: a watch a prompt hook just read is not
        // due again, so there is no refresh window to keep.
        let polled = self.poller.poll_if_stale(TimeDelta::zero(), "serve")?;

        // Delivery happens between the two store operations rather than
        // inside one: a connect is bounded in seconds, and the data lock is
        // what every prompt hook waits on.
        let watches = self.poller.store.read();
        let attached = watches.attached().len();
        let outcomes = round(self.sessions, &watches, self.clock);

        let now = self.clock.now();
        let beat = self.poller.store.update(now, |file| {
            for outcome in &outcomes {
                outcome.apply(file, now);
            }
            Beat {
                pulse: match should_serve(file, now) {
                    true => Pulse::Busy,
                    false => Pulse::Idle,
                },
                next: file.next_read(),
            }
        })?;

        tracing::debug!(
            polled = !polled.skipped,
            watched = polled.watched,
            transitions = polled.events.len(),
            attached,
            probed = outcomes.iter().filter(|o| o.probed).count(),
            written = outcomes.iter().map(|o| o.written).sum::<usize>(),
            detached = outcomes.iter().filter(|o| o.detach).count(),
            elapsed_ms = (self.clock.now() - started).num_milliseconds(),
            pulse = match beat.pulse {
                Pulse::Busy => "busy",
                Pulse::Idle => "idle",
            },
            next = beat.next.map(|t| t.to_rfc3339()),
            "serve tick"
        );
        Ok(beat)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::watches_json::{PUSH_BUDGET, PUSH_WINDOW, RETENTION};

    use crate::domain::watch::{Actors, FailedCheck, Sighting, Standing};
    use crate::secret::Secret;
    use crate::testing::FixedClock;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::io;
    use std::path::PathBuf;

    /// What a fake session does when written to. Listening is the default, so
    /// a test only names the sessions that misbehave.
    #[derive(Clone, Copy)]
    enum Reaction {
        Gone,
        Busy,
    }

    #[derive(Default)]
    struct FakeSessions {
        reactions: RefCell<HashMap<PathBuf, Reaction>>,
        sent: RefCell<Vec<(PathBuf, String)>>,
        probes: RefCell<Vec<PathBuf>>,
    }

    impl FakeSessions {
        fn set(&self, path: &str, reaction: Reaction) {
            self.reactions
                .borrow_mut()
                .insert(PathBuf::from(path), reaction);
        }

        fn answer(&self, address: &SessionAddress) -> Result<(), DeliverError> {
            let path = address.path().to_path_buf();
            match self.reactions.borrow().get(&path).copied() {
                None => Ok(()),
                Some(Reaction::Gone) => Err(DeliverError::Gone {
                    path,
                    reason: "nobody home".into(),
                }),
                Some(Reaction::Busy) => Err(DeliverError::Transient {
                    path,
                    source: io::Error::from(io::ErrorKind::TimedOut),
                }),
            }
        }

        fn texts(&self) -> Vec<String> {
            self.sent.borrow().iter().map(|(_, t)| t.clone()).collect()
        }
    }

    impl Sessions for FakeSessions {
        fn deliver(&self, address: &SessionAddress, text: &str) -> Result<Written, DeliverError> {
            self.answer(address)?;
            self.sent
                .borrow_mut()
                .push((address.path().to_path_buf(), text.to_string()));
            Ok(Written)
        }

        fn probe(&self, address: &SessionAddress) -> Result<(), DeliverError> {
            self.probes.borrow_mut().push(address.path().to_path_buf());
            self.answer(address)
        }
    }

    fn now() -> DateTime<Utc> {
        "2026-09-20T12:00:00Z".parse().unwrap()
    }

    fn clock() -> FixedClock {
        FixedClock::at("2026-09-20T12:00:00Z")
    }

    fn pr(number: u64) -> Reference {
        Reference::GitHubPullRequest {
            host: "github.com".into(),
            owner: "dpep".into(),
            repo: "inkling".into(),
            number,
        }
    }

    fn sighting(state: State, checks: Option<CheckState>) -> Sighting {
        Sighting {
            discussion: None,
            standing: Standing::PullRequest {
                state,
                checks,
                comments: None,
            },
            title: "Add the session socket sink".into(),
            failing: None,
            head: None,
            by: Default::default(),
        }
    }

    fn address(path: &str) -> SessionAddress {
        SessionAddress::new(path, Some(Secret::new("tok")))
    }

    fn forever() -> DateTime<Utc> {
        "2099-01-01T00:00:00Z".parse().unwrap()
    }

    /// One watch, asked for by `owners` and baselined open, with nothing yet
    /// to tell anyone.
    fn watched_by(owners: &[&str]) -> Watches {
        let mut watches = Watches::default();
        for owner in owners {
            watches.watch(&pr(3), owner, forever(), now());
        }
        watches.record(&[(pr(3), sighting(State::Open, None))], now());
        watches
    }

    fn watching() -> Watches {
        watched_by(&["s"])
    }

    fn moved(watches: &mut Watches, state: State) {
        watches.record(&[(pr(3), sighting(state, None))], now());
    }

    /// Run a round and fold it back in, the way a tick does.
    fn deliver(sessions: &FakeSessions, watches: &mut Watches) -> Vec<Outcome> {
        let clock = clock();
        let outcomes = round(sessions, watches, &clock);
        for outcome in &outcomes {
            outcome.apply(watches, now());
        }
        outcomes
    }

    #[test]
    fn each_attached_session_is_told_what_it_alone_has_not_been_told() {
        let mut watches = watched_by(&["early"]);
        watches.attach("early", address("/tmp/a"), now());
        moved(&mut watches, State::Approved);
        // The second session arrives after the approval, so it starts at the
        // head and the merge is the only thing it is owed.
        watches.watch(&pr(3), "late", forever(), now());
        watches.attach("late", address("/tmp/b"), now());
        moved(&mut watches, State::Merged);

        let sessions = FakeSessions::default();
        deliver(&sessions, &mut watches);

        let sent = sessions.sent.borrow();
        let to = |path: &str| -> Vec<String> {
            sent.iter()
                .filter(|(p, _)| p == &PathBuf::from(path))
                .map(|(_, t)| t.clone())
                .collect()
        };
        assert_eq!(
            to("/tmp/a"),
            ["inkling: 2 changes\n\
              - github.com/dpep/inkling#3 | state open → approved\n\
              - github.com/dpep/inkling#3 | state approved → merged"]
        );
        assert_eq!(
            to("/tmp/b"),
            ["inkling: github.com/dpep/inkling#3 | state approved → merged"]
        );
    }

    #[test]
    fn a_pushed_transition_is_not_repeated_at_the_next_prompt() {
        // The promise the whole feed makes, now that there are two ways to
        // deliver: each transition reaches a subscriber once, by whichever
        // got there first.
        let mut watches = watching();
        watches.attach("s", address("/tmp/a"), now());
        moved(&mut watches, State::Approved);

        let sessions = FakeSessions::default();
        deliver(&sessions, &mut watches);
        assert_eq!(sessions.texts().len(), 1);

        assert!(
            watches.take("s", now()).is_empty(),
            "the prompt hook has nothing left to say"
        );
    }

    fn slack() -> Reference {
        Reference::SlackMessage {
            channel: "C0123ABCD".into(),
            ts: "1786596050.245879".into(),
            thread: None,
        }
    }

    fn conversation(reactions: &[(&str, u32)], replies: &[(&str, &str)]) -> Sighting {
        Sighting {
            discussion: None,
            standing: Standing::Message {
                reactions: reactions.iter().map(|(e, n)| (e.to_string(), *n)).collect(),
                replies: replies
                    .iter()
                    .map(|(ts, who)| (ts.to_string(), who.to_string()))
                    .collect(),
            },
            title: String::new(),
            failing: None,
            head: None,
            by: Actors {
                names: [("U1", "alice"), ("U9", "dpepper")]
                    .iter()
                    .map(|(id, name)| (id.to_string(), name.to_string()))
                    .collect(),
                me: Some("U9".into()),
                ..Actors::default()
            },
        }
    }

    fn talking() -> Watches {
        let mut watches = Watches::default();
        watches.watch(&slack(), "s", forever(), now());
        watches.record(&[(slack(), conversation(&[], &[]))], now());
        watches.attach("s", address("/tmp/a"), now());
        watches
    }

    #[test]
    fn a_reply_says_who_and_how_many_and_never_what() {
        let mut watches = talking();
        watches.record(
            &[(
                slack(),
                conversation(&[("white_check_mark", 1)], &[("1.000001", "U1")]),
            )],
            now(),
        );
        let sessions = FakeSessions::default();
        deliver(&sessions, &mut watches);
        assert_eq!(
            sessions.texts(),
            ["inkling: 2 changes\n\
              - slack:C0123ABCD/1786596050.245879 | 1 reply from @alice\n\
              - slack:C0123ABCD/1786596050.245879 | 1 reaction :white_check_mark:"]
        );
    }

    #[test]
    fn what_one_reading_added_to_several_emoji_is_one_line() {
        let mut watches = talking();
        watches.record(
            &[(slack(), conversation(&[("dart", 2), ("+1", 1)], &[]))],
            now(),
        );
        let sessions = FakeSessions::default();
        deliver(&sessions, &mut watches);
        assert_eq!(
            sessions.texts(),
            ["inkling: slack:C0123ABCD/1786596050.245879 | 3 reactions :+1: :dart:"]
        );
    }

    fn event(axis: Axis, from: &str, to: &str, detail: &str) -> Event {
        Event {
            seq: 1,
            at: now(),
            reference: slack(),
            axis,
            from: from.into(),
            to: to.into(),
            title: String::new(),
            detail: Some(detail.into()).filter(|d: &String| !d.is_empty()),
            job: None,
            own: false,
            partial: false,
            audience: Default::default(),
        }
    }

    #[test]
    fn an_accumulating_axis_says_how_many_new_and_from_whom() {
        let said = |e: &Event| super::said(e);
        assert_eq!(
            said(&event(Axis::Replies, "2", "3", "@a")),
            "1 reply from @a"
        );
        assert_eq!(
            said(&event(Axis::Comments, "0", "4", "@a, b (bot), @c")),
            "4 comments from @a, b (bot) and @c"
        );
        assert_eq!(
            said(&event(Axis::Comments, "0", "9", "@a, @b, @c, @d, @e")),
            "9 comments from 5 people",
            "past three, a count rather than a list"
        );
        assert_eq!(
            said(&Event {
                partial: true,
                ..event(Axis::Comments, "1", "41", "@bob")
            }),
            "at least 40 comments from @bob"
        );
        // Stored before the wording changed.
        assert_eq!(
            said(&event(Axis::Replies, "2", "4", "by @alice, @bob")),
            "2 replies from @alice and @bob"
        );
        // A shrink isn't new anything; state-like axes keep from → to.
        assert_eq!(
            said(&event(Axis::Reactions, "1", "0", ":eyes:")),
            "reactions 1 → 0 :eyes:"
        );
        assert_eq!(
            said(&event(Axis::State, "open", "merged", "by @dpep")),
            "state open → merged by @dpep"
        );
    }

    #[test]
    fn a_reply_only_the_reader_wrote_waits_in_the_feed() {
        // Recorded, so a prompt still shows it, but not worth a turn.
        let mut watches = talking();
        watches.record(
            &[(slack(), conversation(&[], &[("1.000001", "U9")]))],
            now(),
        );
        let sessions = FakeSessions::default();
        deliver(&sessions, &mut watches);
        assert!(sessions.texts().is_empty());
        assert_eq!(watches.take("s", now()).len(), 1);
    }

    fn commented(by: &[&str]) -> Sighting {
        let remarks = by
            .iter()
            .enumerate()
            .map(|(i, who)| crate::domain::watch::Remark {
                id: format!("c{i}"),
                at: now() + chrono::TimeDelta::seconds(i as i64),
                by: who.to_string(),
                weight: 1,
                approval: false,
            })
            .collect();
        Sighting {
            discussion: Some(crate::domain::watch::Discussion {
                remarks,
                total: by.len() as u64,
                complete_after: None,
            }),
            by: Actors {
                me: Some("@dpep".into()),
                ..Actors::default()
            },
            ..sighting(State::Open, None)
        }
    }

    #[test]
    fn a_comment_says_who_and_how_many_and_where_to_read_it() {
        let mut watches = Watches::default();
        watches.watch(&pr(42), "s", forever(), now());
        watches.record(&[(pr(42), commented(&["@dpep"]))], now());
        watches.attach("s", address("/tmp/a"), now());

        watches.record(
            &[(
                pr(42),
                commented(&["@dpep", "@alice", "coderabbitai (bot)"]),
            )],
            now(),
        );
        let sessions = FakeSessions::default();
        deliver(&sessions, &mut watches);
        assert_eq!(
            sessions.texts(),
            [
                "inkling: github.com/dpep/inkling#42 | 2 comments from @alice and coderabbitai (bot) \
              (gh pr view 42 --repo github.com/dpep/inkling --comments)"
            ]
        );
    }

    #[test]
    fn a_comment_only_the_reader_wrote_waits_in_the_feed() {
        let mut watches = Watches::default();
        watches.watch(&pr(42), "s", forever(), now());
        watches.record(&[(pr(42), commented(&[]))], now());
        watches.attach("s", address("/tmp/a"), now());

        watches.record(&[(pr(42), commented(&["@dpep"]))], now());
        let sessions = FakeSessions::default();
        deliver(&sessions, &mut watches);
        assert!(sessions.texts().is_empty());
        assert_eq!(watches.take("s", now()).len(), 1);
    }

    fn checked(watches: &mut Watches, checks: CheckState) {
        watches.record(&[(pr(3), sighting(State::Open, Some(checks)))], now());
    }

    #[test]
    fn a_suite_that_has_only_started_waits_for_the_next_prompt() {
        let mut watches = watching();
        watches.attach("s", address("/tmp/a"), now());
        checked(&mut watches, CheckState::Pending);

        let sessions = FakeSessions::default();
        let outcomes = deliver(&sessions, &mut watches);

        assert!(sessions.texts().is_empty(), "{:?}", sessions.texts());
        assert!(
            outcomes[0].probed,
            "silence is still a reason to ask after it"
        );
        assert_eq!(
            watches.take("s", now()).len(),
            1,
            "still there to be pulled"
        );
    }

    #[test]
    fn a_verdict_is_pushed_without_the_start_that_led_to_it() {
        let mut watches = watching();
        watches.attach("s", address("/tmp/a"), now());
        checked(&mut watches, CheckState::Pending);
        checked(&mut watches, CheckState::Success);

        let sessions = FakeSessions::default();
        deliver(&sessions, &mut watches);

        let texts = sessions.texts();
        assert_eq!(texts.len(), 1, "{texts:?}");
        assert!(texts[0].contains("pending → success"), "{texts:?}");
        assert!(
            watches.take("s", now()).is_empty(),
            "the verdict passes over the start; a prompt doesn't dig it back up"
        );
    }

    /// A draft, baselined, watched by `s` with an address.
    fn drafted() -> Watches {
        let mut watches = Watches::default();
        watches.watch(&pr(3), "s", forever(), now());
        watches.record(&[(pr(3), sighting(State::Draft, None))], now());
        watches.attach("s", address("/tmp/a"), now());
        watches
    }

    #[test]
    fn a_draft_marked_ready_waits_for_the_next_prompt() {
        // Dogfooded on #14: `gh pr ready` woke the session that ran it. Out of
        // draft is a start, like a suite starting; review is what follows.
        let mut watches = drafted();
        moved(&mut watches, State::Open);

        let sessions = FakeSessions::default();
        deliver(&sessions, &mut watches);

        assert!(sessions.texts().is_empty(), "{:?}", sessions.texts());
        assert_eq!(
            watches.take("s", now()).len(),
            1,
            "still there to be pulled"
        );
    }

    #[test]
    fn ready_then_merged_costs_one_push_whichever_tick_it_lands_in() {
        // Ready and merge in one tick read `draft → merged`; in two, they read
        // `draft → open`, `open → merged`. What arrives shouldn't depend on it.
        for split in [false, true] {
            let mut watches = drafted();
            if split {
                moved(&mut watches, State::Open);
            }
            moved(&mut watches, State::Merged);

            let sessions = FakeSessions::default();
            deliver(&sessions, &mut watches);

            let texts = sessions.texts();
            assert_eq!(texts.len(), 1, "split={split}: {texts:?}");
            assert!(texts[0].ends_with("→ merged"), "{texts:?}");
            assert!(watches.take("s", now()).is_empty(), "split={split}");
        }
    }

    #[test]
    fn a_pull_request_sent_back_to_draft_is_still_pushed() {
        // Only leaving draft is a start. Going back to it is somebody saying
        // the work isn't ready, which is something to know.
        let mut watches = watching();
        watches.attach("s", address("/tmp/a"), now());
        moved(&mut watches, State::Draft);

        let sessions = FakeSessions::default();
        deliver(&sessions, &mut watches);

        assert_eq!(sessions.texts().len(), 1, "{:?}", sessions.texts());
    }

    #[test]
    fn a_reference_that_flaps_stops_being_pushed() {
        // Measured 2026-09-20 against a live listener: a pull request whose
        // review decision oscillated put seven messages into one session in
        // 13 seconds, one per tick, and nothing was going to stop it until the
        // watch's deadline.
        let mut watches = watching();
        watches.attach("s", address("/tmp/a"), now());
        let sessions = FakeSessions::default();

        for turn in 0..12 {
            moved(
                &mut watches,
                match turn % 2 {
                    0 => State::Approved,
                    _ => State::Open,
                },
            );
            deliver(&sessions, &mut watches);
        }

        assert_eq!(
            sessions.texts().len(),
            PUSH_BUDGET,
            "{:?}",
            sessions.texts()
        );
        assert!(
            !watches.take("s", now()).is_empty(),
            "and what was withheld is still there to be pulled"
        );
    }

    #[test]
    fn the_budget_comes_back_once_the_window_has_passed() {
        let mut watches = watching();
        watches.attach("s", address("/tmp/a"), now());
        let sessions = FakeSessions::default();

        for _ in 0..=PUSH_BUDGET {
            moved(&mut watches, State::Approved);
            moved(&mut watches, State::Open);
            deliver(&sessions, &mut watches);
        }
        assert_eq!(
            sessions.texts().len(),
            PUSH_BUDGET,
            "the last round withheld"
        );

        // A window later the session is told what was withheld.
        let later = now() + PUSH_WINDOW + TimeDelta::seconds(1);
        let clock = FixedClock::at("2026-09-20T12:06:00Z");
        for outcome in round(&sessions, &watches, &clock) {
            outcome.apply(&mut watches, later);
        }
        assert_eq!(sessions.texts().len(), PUSH_BUDGET + 1);
        assert!(sessions.texts()[PUSH_BUDGET].starts_with("inkling: 2 changes"));
    }

    /// Green and attached, so the next reading is the only news.
    fn green_and_attached() -> Watches {
        let mut watches = Watches::default();
        watches.watch(&pr(3), "s", forever(), now());
        read(&mut watches, State::Open, CheckState::Success, 0);
        watches.attach("s", address("/tmp/a"), now());
        watches
    }

    fn read(watches: &mut Watches, state: State, checks: CheckState, seconds: i64) {
        let at = now() + TimeDelta::seconds(seconds);
        watches.record(&[(pr(3), sighting(state, Some(checks)))], at);
    }

    #[test]
    fn red_waits_for_a_second_reading_that_still_says_red() {
        let mut watches = green_and_attached();
        read(&mut watches, State::Open, CheckState::Failure, 10);
        read(&mut watches, State::Approved, CheckState::Failure, 20);

        let sessions = FakeSessions::default();
        deliver(&sessions, &mut watches);
        assert!(
            sessions.texts().is_empty(),
            "red not yet known to last, and the approval queues behind it: {:?}",
            sessions.texts()
        );

        // Read again a minute on, still red: now it is news.
        read(&mut watches, State::Approved, CheckState::Failure, 70);
        deliver(&sessions, &mut watches);
        let texts = sessions.texts();
        assert_eq!(texts.len(), 1, "{texts:?}");
        assert!(texts[0].contains("success → failure"), "{texts:?}");
        assert!(texts[0].contains("open → approved"), "{texts:?}");
    }

    #[test]
    fn red_that_a_retry_cleared_is_never_pushed() {
        // Buildkite marks a build red while a retry is still running, and the
        // retry passing takes it back.
        let mut watches = green_and_attached();
        let sessions = FakeSessions::default();

        read(&mut watches, State::Open, CheckState::Failure, 10);
        deliver(&sessions, &mut watches);
        read(&mut watches, State::Open, CheckState::Pending, 70);
        deliver(&sessions, &mut watches);
        assert!(sessions.texts().is_empty(), "{:?}", sessions.texts());

        read(&mut watches, State::Open, CheckState::Success, 300);
        deliver(&sessions, &mut watches);
        assert_eq!(
            sessions.texts(),
            ["inkling: github.com/dpep/inkling#3 | checks pending → success"]
        );
    }

    #[test]
    fn red_on_a_watch_that_is_gone_is_pushed_rather_than_held_forever() {
        let mut watches = green_and_attached();
        read(&mut watches, State::Open, CheckState::Failure, 10);
        watches.watches.clear();

        let sessions = FakeSessions::default();
        deliver(&sessions, &mut watches);
        assert_eq!(sessions.texts().len(), 1, "{:?}", sessions.texts());
    }

    #[test]
    fn a_gone_address_is_detached_and_its_events_stay_pullable() {
        // A session that exited without detaching. The push is lost, the news
        // is not: it is still owed to that cursor when the session comes back.
        let mut watches = watching();
        watches.attach("s", address("/tmp/a"), now());
        moved(&mut watches, State::Approved);

        let sessions = FakeSessions::default();
        sessions.set("/tmp/a", Reaction::Gone);
        let outcomes = deliver(&sessions, &mut watches);

        assert!(outcomes[0].detach);
        assert!(watches.attached().is_empty(), "nowhere left to push");
        assert_eq!(
            watches.take("s", now()).len(),
            1,
            "and the transition survived the failed push"
        );
    }

    #[test]
    fn a_busy_session_keeps_both_its_address_and_its_place() {
        // A timeout says nothing about whether the session is still there, so
        // the next tick tries the same message again.
        let mut watches = watching();
        watches.attach("s", address("/tmp/a"), now());
        moved(&mut watches, State::Approved);

        let sessions = FakeSessions::default();
        sessions.set("/tmp/a", Reaction::Busy);
        deliver(&sessions, &mut watches);

        assert_eq!(watches.attached().len(), 1);
        assert_eq!(watches.attached()[0].2, 0, "the cursor did not move");
    }

    #[test]
    fn delivery_stops_at_the_first_failure() {
        // The cursor is one number: writing the third message after the
        // second failed would mark the second delivered along with it.
        let mut watches = watching();
        watches.attach("s", address("/tmp/a"), now());
        moved(&mut watches, State::Approved);
        moved(&mut watches, State::Merged);

        let sessions = FakeSessions::default();
        sessions.set("/tmp/a", Reaction::Busy);
        deliver(&sessions, &mut watches);

        assert!(sessions.texts().is_empty());
        assert_eq!(
            watches.take("s", now()).len(),
            2,
            "both are still owed, in order"
        );
    }

    #[test]
    fn a_subscriber_owed_nothing_is_asked_after_rather_than_written_to() {
        let mut watches = watching();
        watches.attach("s", address("/tmp/a"), now());

        let sessions = FakeSessions::default();
        let outcomes = deliver(&sessions, &mut watches);

        assert!(outcomes[0].probed);
        assert_eq!(sessions.probes.borrow().len(), 1);
        assert!(
            sessions.texts().is_empty(),
            "a probe that said something would start a turn"
        );
    }

    #[test]
    fn a_probe_that_finds_nobody_detaches_before_there_is_news_to_lose() {
        let mut watches = watching();
        watches.attach("s", address("/tmp/a"), now());

        let sessions = FakeSessions::default();
        sessions.set("/tmp/a", Reaction::Gone);
        deliver(&sessions, &mut watches);

        assert!(watches.attached().is_empty());
        assert!(
            watches.subscribers.contains_key("s"),
            "the cursor outlives the address"
        );
    }

    #[test]
    fn a_server_needs_both_a_listener_and_something_that_can_still_move() {
        let mut watches = watching();
        assert!(!should_serve(&watches, now()), "nobody is listening");

        watches.attach("s", address("/tmp/a"), now());
        assert!(should_serve(&watches, now()));

        // The pull request merges, so its watch drops itself.
        moved(&mut watches, State::Merged);
        assert!(
            !should_serve(&watches, now()),
            "nothing left that could move"
        );
    }

    #[test]
    fn an_expired_watch_is_not_a_reason_to_keep_serving() {
        let mut watches = Watches::default();
        watches.watch(&pr(3), "s", now() - TimeDelta::minutes(1), now());
        watches.attach("s", address("/tmp/a"), now());

        assert!(!should_serve(&watches, now()));
    }

    #[test]
    fn a_message_says_what_moved_and_nothing_anyone_else_wrote() {
        let mut watches = watching();
        moved(&mut watches, State::Approved);
        let text = message(&watches.events[0]);

        assert_eq!(
            text,
            "inkling: github.com/dpep/inkling#3 | state open → approved"
        );
        assert!(
            !text.contains("session socket"),
            "the title is somebody else's prose: {text}"
        );
    }

    #[test]
    fn an_approval_says_who_gave_it() {
        let mut watches = watching();
        watches.record(
            &[(
                pr(3),
                Sighting {
                    by: Actors {
                        approved: vec!["@alice".into(), "buildsworth (bot)".into()],
                        ..Default::default()
                    },
                    ..sighting(State::Approved, None)
                },
            )],
            now(),
        );
        assert_eq!(
            message(&watches.events[0]),
            "inkling: github.com/dpep/inkling#3 | state open → approved by @alice / buildsworth (bot)"
        );
    }

    fn went_red(job: Option<u64>) -> String {
        let mut watches = watching();
        watches.record(
            &[(pr(3), sighting(State::Open, Some(CheckState::Pending)))],
            now(),
        );
        watches.record(
            &[(
                pr(3),
                Sighting {
                    failing: Some(FailedCheck {
                        name: "test / build".into(),
                        job,
                    }),
                    ..sighting(State::Open, Some(CheckState::Failure))
                },
            )],
            now(),
        );
        message(watches.events.last().unwrap())
    }

    #[test]
    fn a_red_actions_job_points_at_its_log_which_is_readable_before_the_run_ends() {
        assert_eq!(
            went_red(Some(106192233819)),
            "inkling: github.com/dpep/inkling#3 | checks pending → failure: test / build \
             (gh api --hostname github.com repos/dpep/inkling/actions/jobs/106192233819/logs)"
        );
    }

    #[test]
    fn a_red_check_with_no_job_behind_it_points_at_the_list_of_checks() {
        assert_eq!(
            went_red(None),
            "inkling: github.com/dpep/inkling#3 | checks pending → failure: test / build \
             (gh pr checks 3 --repo github.com/dpep/inkling)"
        );
    }

    #[test]
    fn a_green_suite_suggests_nothing_because_there_is_nothing_to_look_at() {
        let mut watches = watching();
        watches.record(
            &[(pr(3), sighting(State::Open, Some(CheckState::Pending)))],
            now(),
        );
        watches.record(
            &[(pr(3), sighting(State::Open, Some(CheckState::Success)))],
            now(),
        );

        let text = message(watches.events.last().unwrap());
        assert_eq!(
            text,
            "inkling: github.com/dpep/inkling#3 | checks pending → success"
        );
    }

    #[test]
    fn an_issue_reads_the_same_way_without_a_command() {
        let mut watches = Watches::default();
        let key = Reference::JiraIssue { key: "AI-1".into() };
        watches.watch(&key, "s", forever(), now());
        let issue = |status: &str| Sighting {
            standing: Standing::Issue {
                status: status.into(),
                category: crate::domain::watch::Category::Indeterminate,
            },
            title: "Try the watch feature".into(),
            failing: None,
            head: None,
            by: Default::default(),
            discussion: None,
        };
        watches.record(&[(key.clone(), issue("To Do"))], now());
        watches.record(&[(key, issue("In Review"))], now());

        assert_eq!(
            message(&watches.events[0]),
            "inkling: AI-1 | status To Do → In Review"
        );
    }

    #[test]
    fn a_detached_subscriber_keeps_its_cursor_for_a_week_of_silence() {
        // The idle session this feature is for: it pushed a PR and is waiting,
        // so it says nothing for hours. Its address must not age out from
        // under it on the event log's clock.
        let mut watches = watching();
        watches.attach("s", address("/tmp/a"), now());

        watches.collect_garbage(now() + RETENTION + TimeDelta::hours(1), RETENTION);
        assert_eq!(watches.attached().len(), 1);
    }
}
