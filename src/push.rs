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
use crate::serve::{Pulse, Ticked, Work};
use crate::watch::Poller;
use chrono::{DateTime, TimeDelta, Utc};

/// What one transition says when it arrives in a session.
///
/// Built only from fields inkling controls — the reference, the axis, the two
/// standings, the name of the check that went red. Deliberately not the pull
/// request's title or the issue's summary: this lands inside an agent's turn,
/// and prose somebody else wrote does not belong there.
pub fn message(event: &Event) -> String {
    let mut text = format!(
        "inkling: {} {} {} → {}",
        event.reference, event.axis, event.from, event.to
    );
    if let Some(detail) = &event.detail {
        text.push_str(&format!(": {detail}"));
    }
    if let Some(command) = more(event) {
        text.push_str(&format!(" ({command})"));
    }
    text
}

/// The one command that says more, in the one case where there obviously is
/// one: checks went red, and the next question is always why.
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
        Axis::Status => false,
    };
    !started
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
            let owed: Vec<&Event> = addressed(&watches.events, cursor, subscriber)
                .into_iter()
                .filter(|event| worth_a_turn(event))
                .collect();
            // A session is only ever told so much. What the budget withholds
            // stays in the feed for the next prompt, so this caps the turns
            // inkling can cost without dropping anything.
            let allowance = watches.allowance(subscriber, clock.now());
            if owed.len() > allowance {
                tracing::info!(
                    subscriber,
                    owed = owed.len(),
                    allowance,
                    window_minutes = crate::adapters::watches_json::PUSH_WINDOW.num_minutes(),
                    "withheld; the rest waits for a prompt"
                );
            }
            let due: Vec<&Event> = owed.into_iter().take(allowance).collect();
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
    for event in due {
        let started = clock.now();
        let sent = sessions.deliver(address, &message(event));
        let elapsed_ms = (clock.now() - started).num_milliseconds();

        match sent {
            Ok(Written) => {
                outcome.cursor = Some(event.seq);
                outcome.written += 1;
                tracing::info!(
                    subscriber,
                    seq = event.seq,
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
                    seq = event.seq,
                    outcome = match outcome.detach {
                        true => "gone",
                        false => "transient",
                    },
                    elapsed_ms,
                    advanced = false,
                    %error,
                    "pushed"
                );
                // Stop at the first failure. The cursor is one number, so
                // skipping past a message that didn't arrive would mark it
                // delivered along with the ones that did.
                break;
            }
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
        let pulse = self.poller.store.update(now, |file| {
            for outcome in &outcomes {
                outcome.apply(file, now);
            }
            match should_serve(file, now) {
                true => Pulse::Busy,
                false => Pulse::Idle,
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
            pulse = match pulse {
                Pulse::Busy => "busy",
                Pulse::Idle => "idle",
            },
            "serve tick"
        );
        Ok(pulse)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::watches_json::{PUSH_BUDGET, PUSH_WINDOW, RETENTION};

    use crate::domain::watch::{FailedCheck, Sighting, Standing};
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
            standing: Standing::PullRequest { state, checks },
            title: "Add the session socket sink".into(),
            failing: None,
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
        assert_eq!(to("/tmp/a").len(), 2, "approved and merged");
        assert_eq!(to("/tmp/b").len(), 1, "{:?}", to("/tmp/b"));
        assert!(to("/tmp/b")[0].contains("approved → merged"));
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

        for _ in 0..PUSH_BUDGET {
            moved(&mut watches, State::Approved);
            moved(&mut watches, State::Open);
            deliver(&sessions, &mut watches);
        }
        assert_eq!(sessions.texts().len(), PUSH_BUDGET);

        // A window later the session can be told things again — and still no
        // more than a budget's worth of them.
        let later = now() + PUSH_WINDOW + TimeDelta::seconds(1);
        let clock = FixedClock::at("2026-09-20T12:06:00Z");
        for outcome in round(&sessions, &watches, &clock) {
            outcome.apply(&mut watches, later);
        }
        assert_eq!(sessions.texts().len(), PUSH_BUDGET * 2);
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
            "inkling: github.com/dpep/inkling#3 state open → approved"
        );
        assert!(
            !text.contains("session socket"),
            "the title is somebody else's prose: {text}"
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
            "inkling: github.com/dpep/inkling#3 checks pending → failure: test / build \
             (gh api --hostname github.com repos/dpep/inkling/actions/jobs/106192233819/logs)"
        );
    }

    #[test]
    fn a_red_check_with_no_job_behind_it_points_at_the_list_of_checks() {
        assert_eq!(
            went_red(None),
            "inkling: github.com/dpep/inkling#3 checks pending → failure: test / build \
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
            "inkling: github.com/dpep/inkling#3 checks pending → success"
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
        };
        watches.record(&[(key.clone(), issue("To Do"))], now());
        watches.record(&[(key, issue("In Review"))], now());

        assert_eq!(
            message(&watches.events[0]),
            "inkling: AI-1 status To Do → In Review"
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
