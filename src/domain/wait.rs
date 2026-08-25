//! What it means to be waiting for something, and when the waiting is over.
//!
//! Pure. Given a set of observations it says met, precluded, or keep going —
//! the loop, the sleeping, and the network live elsewhere. Everything here
//! falls out of the two enums' orderings, which is the point: `State` already
//! knows that `Closed` is the bottom, so "this can never reach merged now"
//! needs no rule of its own.

use crate::domain::model::{Reference, State};
use std::fmt;
use std::str::FromStr;

/// Where CI stands on a commit.
///
/// Ordered like `State`, and for the same reason: the aggregate over several
/// references is the minimum, so the worst news wins. `Failure` sits lowest
/// because a red suite is decisive — the caller waiting to "respond when CI
/// finishes" can respond the moment one thing goes red, without waiting on
/// whatever is still running.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Failure,
    Pending,
    Success,
}

impl CheckState {
    /// Whether the run has stopped moving.
    ///
    /// Unlike `State::is_terminal`, this is *not* permanent — a push or a
    /// re-run takes a finished suite back to pending. That's survivable here
    /// because a wait evaluates the observation in front of it and then ends.
    /// It would not be survivable in the tracked snapshot, where terminal
    /// means "stop polling forever", which is why check state never goes
    /// there.
    pub fn is_settled(self) -> bool {
        matches!(self, CheckState::Failure | CheckState::Success)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            CheckState::Failure => "failure",
            CheckState::Pending => "pending",
            CheckState::Success => "success",
        }
    }
}

impl fmt::Display for CheckState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// The condition a wait is waiting for.
///
/// Deliberately tiny. Two threshold shapes and a settling shape cover both
/// motivating cases; anything richer is the expression language that turns
/// this into a rules engine (DESIGN §1 rules that out). A new predicate has
/// to argue its way in here, as a named variant, never as a syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Until {
    /// Moved at all, in any direction — whatever the next step turns out to
    /// be. The one condition that needs no guess about what you're waiting
    /// for, which is why it's the default.
    Changed,
    /// Reached at least this far along the lifecycle.
    Reached(State),
    /// Reached any terminal state — merged or closed, whichever comes.
    Done,
    /// CI finished, green or red.
    Checks,
}

impl Until {
    /// The names `--until` accepts.
    ///
    /// `closed` is absent on purpose: the predicate is "reached at least X",
    /// and `Closed` ranks lowest, so `--until closed` would be true the
    /// instant it was asked. `done` is the honest spelling of that intent.
    pub const NAMES: [&'static str; 7] = [
        "changed", "merged", "approved", "open", "draft", "done", "checks",
    ];
}

impl fmt::Display for Until {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Until::Changed => f.pad("changed"),
            Until::Reached(state) => f.pad(state.as_str()),
            Until::Done => f.pad("done"),
            Until::Checks => f.pad("checks"),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UnknownUntil {
    #[error(
        "--until closed can't be waited for: every state counts as \"at least closed\". \
             Use --until done to wait for it to end either way."
    )]
    Closed,
    #[error("unknown --until {name:?}; expected one of: {}", Until::NAMES.join(", "))]
    Unknown { name: String },
}

impl FromStr for Until {
    type Err = UnknownUntil;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "changed" => Ok(Until::Changed),
            "done" => Ok(Until::Done),
            "checks" => Ok(Until::Checks),
            // Caught by name rather than by rank: the reason is specific and
            // the fix is a different word, so saying so beats "unknown".
            "closed" => Err(UnknownUntil::Closed),
            other => {
                State::from_str(other)
                    .map(Until::Reached)
                    .map_err(|_| UnknownUntil::Unknown {
                        name: other.to_string(),
                    })
            }
        }
    }
}

/// What one poll saw about one reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub reference: Reference,
    pub state: State,
    /// `None` when the repository has no checks configured at all — which is
    /// different from having them and them not having started.
    pub checks: Option<CheckState>,
    pub title: String,
}

/// How a wait ended, or that it hasn't.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// The condition holds.
    Met,
    /// The condition can no longer be met — the PR closed while you waited
    /// for it to merge, or CI came back red.
    Precluded,
    /// Nothing yet.
    Pending,
}

/// The lifecycle position of a whole set of references.
///
/// The minimum, which is the same fold reconciliation uses for a message
/// holding several links: one closed pull request pins the group down.
pub fn aggregate(observations: &[Observation]) -> Option<State> {
    observations.iter().map(|o| o.state).min()
}

/// Judge a full set of observations against the condition.
///
/// All references must satisfy it. `baseline` is the aggregate the wait
/// started from, which only `Changed` consults — everything else is a
/// statement about where things are, not about where they were.
///
/// An empty set is `Pending` and never resolves — the caller rejects that
/// earlier, where it can say something useful about it.
pub fn resolve(until: Until, observations: &[Observation], baseline: Option<State>) -> Resolution {
    if observations.is_empty() {
        return Resolution::Pending;
    }

    match until {
        Until::Checks => match observations.iter().map(|o| o.checks).min().flatten() {
            // A repo with no checks would otherwise spin for the full
            // timeout waiting for a suite that is never going to run.
            None => Resolution::Precluded,
            Some(CheckState::Success) => Resolution::Met,
            Some(CheckState::Failure) => Resolution::Precluded,
            Some(CheckState::Pending) => Resolution::Pending,
        },
        _ => {
            let aggregate = aggregate(observations).expect("non-empty");
            match until {
                // Nothing follows Merged, and Inkling treats Closed as
                // finished too — so an unchanged terminal reference is not
                // slow news, it's the end of the story.
                Until::Changed => match baseline {
                    Some(was) if was != aggregate => Resolution::Met,
                    _ if aggregate.is_terminal() => Resolution::Precluded,
                    _ => Resolution::Pending,
                },
                Until::Done if aggregate.is_terminal() => Resolution::Met,
                Until::Reached(threshold) if aggregate >= threshold => Resolution::Met,
                // Terminal and still short of the mark: nothing that happens
                // next can help. Falls straight out of the ordering rather
                // than being a case anyone had to write down.
                _ if aggregate.is_terminal() => Resolution::Precluded,
                _ => Resolution::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(number: u64) -> Reference {
        Reference::GitHubPullRequest {
            host: "github.com".into(),
            owner: "dpep".into(),
            repo: "inkling".into(),
            number,
        }
    }

    fn seen(number: u64, state: State) -> Observation {
        Observation {
            reference: reference(number),
            state,
            checks: None,
            title: String::new(),
        }
    }

    fn checked(number: u64, checks: CheckState) -> Observation {
        Observation {
            checks: Some(checks),
            ..seen(number, State::Open)
        }
    }

    #[test]
    fn a_threshold_is_met_at_or_above_it() {
        for state in [State::Approved, State::Merged] {
            assert_eq!(
                resolve(Until::Reached(State::Approved), &[seen(1, state)], None),
                Resolution::Met,
                "{state} should satisfy approved"
            );
        }
        assert_eq!(
            resolve(
                Until::Reached(State::Approved),
                &[seen(1, State::Open)],
                None
            ),
            Resolution::Pending
        );
    }

    #[test]
    fn closing_a_pull_request_precludes_waiting_for_it_to_merge() {
        // The case the exit-code table exists for: don't report a timeout
        // when the answer is already known and it's no.
        assert_eq!(
            resolve(
                Until::Reached(State::Merged),
                &[seen(1, State::Closed)],
                None
            ),
            Resolution::Precluded
        );
    }

    #[test]
    fn a_draft_is_still_pending_because_it_can_still_progress() {
        assert_eq!(
            resolve(
                Until::Reached(State::Merged),
                &[seen(1, State::Draft)],
                None
            ),
            Resolution::Pending
        );
    }

    #[test]
    fn done_accepts_either_terminal_state() {
        assert_eq!(
            resolve(Until::Done, &[seen(1, State::Merged)], None),
            Resolution::Met
        );
        assert_eq!(
            resolve(Until::Done, &[seen(1, State::Closed)], None),
            Resolution::Met
        );
        assert_eq!(
            resolve(Until::Done, &[seen(1, State::Open)], None),
            Resolution::Pending
        );
    }

    #[test]
    fn several_references_must_all_satisfy_the_condition() {
        let mixed = [seen(1, State::Merged), seen(2, State::Open)];
        assert_eq!(
            resolve(Until::Reached(State::Merged), &mixed, None),
            Resolution::Pending,
            "one merged and one open is not done"
        );

        let both = [seen(1, State::Merged), seen(2, State::Merged)];
        assert_eq!(
            resolve(Until::Reached(State::Merged), &both, None),
            Resolution::Met
        );
    }

    #[test]
    fn one_closed_sibling_precludes_the_whole_wait() {
        // Same aggregation reconcile uses: Closed ranks lowest, so it pins
        // the minimum and the group can never reach the threshold.
        let refs = [seen(1, State::Merged), seen(2, State::Closed)];
        assert_eq!(
            resolve(Until::Reached(State::Merged), &refs, None),
            Resolution::Precluded
        );
    }

    #[test]
    fn a_red_suite_ends_the_wait_immediately() {
        // "so i can respond" — one failure is enough to respond to, even
        // with other jobs still running.
        let refs = [
            checked(1, CheckState::Failure),
            checked(2, CheckState::Pending),
        ];
        assert_eq!(resolve(Until::Checks, &refs, None), Resolution::Precluded);
    }

    #[test]
    fn checks_are_met_only_once_every_reference_is_green() {
        assert_eq!(
            resolve(Until::Checks, &[checked(1, CheckState::Success)], None),
            Resolution::Met
        );
        let refs = [
            checked(1, CheckState::Success),
            checked(2, CheckState::Pending),
        ];
        assert_eq!(resolve(Until::Checks, &refs, None), Resolution::Pending);
    }

    #[test]
    fn a_repository_without_checks_resolves_rather_than_spinning() {
        assert_eq!(
            resolve(Until::Checks, &[seen(1, State::Open)], None),
            Resolution::Precluded,
            "no checks configured is an answer, not a reason to wait"
        );
    }

    #[test]
    fn waiting_on_checks_ignores_the_lifecycle_state() {
        // A merged PR with a red suite is still red. Checks are their own
        // axis — that's why they aren't a rung on State.
        let merged_but_red = Observation {
            checks: Some(CheckState::Failure),
            ..seen(1, State::Merged)
        };
        assert_eq!(
            resolve(Until::Checks, &[merged_but_red], None),
            Resolution::Precluded
        );
    }

    #[test]
    fn changed_resolves_on_any_move_in_any_direction() {
        // The point of the default: you don't have to know which way it
        // goes. Approval, merge, close, or a draft opening all count.
        for now in [State::Approved, State::Merged, State::Closed, State::Open] {
            assert_eq!(
                resolve(Until::Changed, &[seen(1, now)], Some(State::Draft)),
                Resolution::Met,
                "draft -> {now} is a change"
            );
        }
    }

    #[test]
    fn changed_is_pending_while_nothing_moves() {
        assert_eq!(
            resolve(Until::Changed, &[seen(1, State::Open)], Some(State::Open)),
            Resolution::Pending
        );
    }

    #[test]
    fn changed_on_something_already_finished_says_so_rather_than_waiting() {
        // Nothing follows a terminal state, so "wait for the next step" has
        // no next step to wait for. Precluded beats spinning for the full
        // timeout to reach the same conclusion.
        for done in [State::Merged, State::Closed] {
            assert_eq!(
                resolve(Until::Changed, &[seen(1, done)], Some(done)),
                Resolution::Precluded,
                "{done} has no next step"
            );
        }
    }

    #[test]
    fn changed_needs_a_baseline_before_it_can_judge() {
        // Without one, every reading would look like news.
        assert_eq!(
            resolve(Until::Changed, &[seen(1, State::Open)], None),
            Resolution::Pending
        );
    }

    #[test]
    fn changed_watches_the_aggregate_not_any_one_reference() {
        // Two references, one moves: the group's minimum moved, so the wait
        // is over. Same fold as everything else.
        let before = aggregate(&[seen(1, State::Open), seen(2, State::Approved)]);
        assert_eq!(before, Some(State::Open));

        let after = [seen(1, State::Approved), seen(2, State::Approved)];
        assert_eq!(resolve(Until::Changed, &after, before), Resolution::Met);
    }

    #[test]
    fn until_closed_is_refused_with_the_word_that_works() {
        // A silent accept would be worse than an error: `--until closed` is
        // trivially true, so it would exit 0 instantly and look like news.
        assert_eq!("closed".parse::<Until>(), Err(UnknownUntil::Closed));
        assert!(
            UnknownUntil::Closed.to_string().contains("--until done"),
            "the error has to name the fix"
        );
    }

    #[test]
    fn every_advertised_until_name_parses() {
        for name in Until::NAMES {
            assert!(name.parse::<Until>().is_ok(), "{name} is advertised");
        }
        assert!("nonsense".parse::<Until>().is_err());
    }

    #[test]
    fn until_names_round_trip() {
        for name in Until::NAMES {
            assert_eq!(name.parse::<Until>().unwrap().to_string(), name);
        }
    }

    #[test]
    fn check_states_rank_failure_lowest_so_the_worst_news_wins() {
        assert!(CheckState::Failure < CheckState::Pending);
        assert!(CheckState::Pending < CheckState::Success);
        assert!(!CheckState::Pending.is_settled());
    }
}
