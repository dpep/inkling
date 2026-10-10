//! What it means to watch something, and what counts as news.
//!
//! Pure. A *standing* is where a reference is right now; a *transition* is the
//! difference between two standings, and that difference — not the standing —
//! is what a subscriber is told about. The first standing observed for a watch
//! establishes the baseline and is deliberately silent: the caller has just
//! asked to watch the thing, so where it already sits is not news.
//!
//! Two reference kinds stand in different places — a pull request on the merge
//! ladder plus a check rollup, an issue on its project's status board — so
//! `Standing` is a sum, not a lowest common denominator. Flattening them onto
//! one enum would either lose the status name, which is the whole content of a
//! Jira event, or bolt a spare field onto every pull request.

use crate::domain::model::{Reference, State};
use crate::domain::wait::CheckState;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;

/// Jira's `statusCategory.key`. The only ordering Jira guarantees across
/// projects — status *names* are per-project prose, so nothing may be
/// inferred from them beyond equality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    New,
    Indeterminate,
    Done,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Category::New => "new",
            Category::Indeterminate => "indeterminate",
            Category::Done => "done",
        }
    }
}

/// Where a watched reference stands, as of one reading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Standing {
    PullRequest {
        state: State,
        /// `None` when the head commit has no check rollup — no checks
        /// configured, or none created yet. See `carried_over`.
        checks: Option<CheckState>,
    },
    Issue {
        status: String,
        category: Category,
    },
}

impl Standing {
    /// Whether anything more can happen. A finished watch stops being polled.
    ///
    /// Only the lifecycle decides this. Check state settles and unsettles —
    /// red, then pending on a re-run, then green is three real events — so
    /// while the pull request is open the watch stays live.
    pub fn is_finished(&self) -> bool {
        match self {
            Standing::PullRequest { state, .. } => state.is_terminal(),
            Standing::Issue { category, .. } => *category == Category::Done,
        }
    }
}

/// Which axis moved. A pull request has two that move independently, which is
/// why one poll can produce two events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    /// The merge ladder: draft, open, approved, merged, closed.
    State,
    /// The CI rollup on the head commit.
    Checks,
    /// A Jira issue's status.
    Status,
}

impl Axis {
    pub fn as_str(self) -> &'static str {
        match self {
            Axis::State => "state",
            Axis::Checks => "checks",
            Axis::Status => "status",
        }
    }
}

impl fmt::Display for Axis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// A check that is red, as a provider names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedCheck {
    pub name: String,
    /// The GitHub Actions job behind it, when it is one. Its log can be read
    /// the moment the job ends; `gh run view` refuses until the whole run does.
    pub job: Option<u64>,
}

/// Where a pull request's commits come from: the repository and branch a
/// `git push` has to name for the push to be to this pull request.
///
/// The repository matters as much as the branch. `main` or `fix` from a
/// stranger's fork is a different pull request with the same branch name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Head {
    /// `owner/name`, as GitHub spells `nameWithOwner`.
    pub repo: String,
    pub branch: String,
}

impl Head {
    /// Owners and repository names are case-insensitive on GitHub; branch
    /// names are not.
    pub fn is(&self, repo: &str, branch: &str) -> bool {
        self.repo.eq_ignore_ascii_case(repo) && self.branch == branch
    }
}

/// One reading of one reference, as a poll hands it over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sighting {
    pub standing: Standing,
    /// What the thing is called, so an event reads without a second lookup.
    pub title: String,
    /// A pull request's head, when the provider could name it. Remembered on
    /// the watch so a later push to it is recognised without asking.
    pub head: Option<Head>,
    /// Which check is red, when one is and the provider could name it.
    ///
    /// Deliberately outside `Standing`: a different job failing is the same
    /// news, and including it in the comparison would emit an event every
    /// time the first red job changed name.
    pub failing: Option<FailedCheck>,
    /// Outside `Standing` for the same reason as `failing`: a second
    /// approver is not a transition.
    pub by: Actors,
}

/// Who moved a pull request, as `@login`, or `login (bot)` for an app: a
/// review bot satisfying the requirement, or automation sending a pull
/// request back to draft, is different news from a person.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Actors {
    /// Whose approvals stand.
    pub approved: Vec<String>,
    /// Who last marked it ready or converted it to draft.
    pub draft: Option<String>,
    /// Who merged it.
    pub merged: Option<String>,
}

impl Actors {
    /// Who put a pull request in the state it is now in, if anyone can say.
    /// Only the end state counts: whoever readied a PR on its way to merged
    /// is not who merged it.
    fn of(&self, was: State, now: State) -> Option<String> {
        let who = match now {
            State::Merged => self.merged.clone()?,
            State::Approved if !self.approved.is_empty() => self.approved.join(" / "),
            State::Draft => self.draft.clone()?,
            State::Open if was == State::Draft => self.draft.clone()?,
            _ => return None,
        };
        Some(format!("by {who}"))
    }
}

/// A change worth telling somebody about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub axis: Axis,
    pub from: String,
    pub to: String,
    /// Extra the axis alone doesn't carry — the name of the failing check.
    pub detail: Option<String>,
    /// The Actions job behind that check, so its log is one command away.
    pub job: Option<u64>,
}

/// A transition, once it has a place in the sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Monotonic across the whole log. The cursor is a sequence number, so a
    /// subscriber's position survives pruning and reordering alike.
    pub seq: u64,
    pub at: DateTime<Utc>,
    pub reference: Reference,
    pub axis: Axis,
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<u64>,
    /// Who was watching this reference when it moved.
    ///
    /// Snapshotted rather than looked up later: a finished watch is removed,
    /// and a reader that stops caring stops from then on rather than
    /// retrospectively. Empty only happens for an event written before
    /// watches had owners, and reaches nobody.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub audience: BTreeSet<String>,
}

/// What one reading amounts to, given the reading before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    /// Where the reference stands now: this sighting, with any axis it did not
    /// observe carried over from the standing before it.
    pub standing: Standing,
    /// What moved. Empty on a baseline, and empty when nothing did.
    pub moved: Vec<Transition>,
}

/// Compare a sighting against the standing before it.
///
/// `before` is `None` on the first sighting of a watch, which yields nothing:
/// a baseline is not an event. Without that rule every new watch would
/// announce a state its owner had just looked up.
///
/// The standing comes back *with* the transitions rather than beside them,
/// because the two have to agree: storing a standing other than the one that
/// was compared would diff the next reading against something nobody was ever
/// told about.
pub fn reading(before: Option<&Standing>, after: &Sighting) -> Reading {
    let standing = carried_over(before, &after.standing);
    let Some(before) = before else {
        return Reading {
            standing,
            moved: Vec::new(),
        };
    };
    let moved = match (before, &standing) {
        (
            Standing::PullRequest {
                state: was,
                checks: had,
            },
            Standing::PullRequest {
                state: now,
                checks: has,
            },
        ) => {
            let mut moved = Vec::new();
            if was != now {
                moved.push(Transition {
                    axis: Axis::State,
                    from: was.as_str().into(),
                    to: now.as_str().into(),
                    detail: after.by.of(*was, *now),
                    job: None,
                });
            }
            if had != has {
                moved.push(Transition {
                    axis: Axis::Checks,
                    from: checks_label(*had).into(),
                    to: checks_label(*has).into(),
                    detail: after.failing.as_ref().map(|f| f.name.clone()),
                    job: after.failing.as_ref().and_then(|f| f.job),
                });
            }
            moved
        }
        (Standing::Issue { status: was, .. }, Standing::Issue { status: now, .. })
            if was != now =>
        {
            vec![Transition {
                axis: Axis::Status,
                from: was.clone(),
                to: now.clone(),
                detail: None,
                job: None,
            }]
        }
        // Same status, or a reference that somehow changed kind — which it
        // can't, since a watch is keyed by the reference itself.
        _ => Vec::new(),
    };
    Reading { standing, moved }
}

/// A pull request whose head commit has no check rollup keeps the rollup it
/// had.
///
/// `None` means both "this repository runs no checks" and "the commit pushed a
/// second ago has no suite yet", and one reading cannot tell them apart. Once a
/// rollup has been seen, its absence is a gap in our observation rather than a
/// standing: carrying it over makes the feed report what it would have reported
/// had the suite appeared at once, so what arrives no longer depends on whether
/// a poll happened to land in the gap.
///
/// Not symmetric, on purpose. A rollup we have never seen becoming one we have
/// is news — suppressing that too would silence a suite that goes straight to
/// red on a pull request watched before its first check existed.
fn carried_over(before: Option<&Standing>, after: &Standing) -> Standing {
    match (before, after) {
        (
            Some(Standing::PullRequest {
                checks: Some(had), ..
            }),
            Standing::PullRequest {
                state,
                checks: None,
            },
        ) => Standing::PullRequest {
            state: *state,
            checks: Some(*had),
        },
        _ => after.clone(),
    }
}

/// `None` is a pull request with no check rollup, which reads better as a word
/// than as an absence in `none → pending`.
fn checks_label(checks: Option<CheckState>) -> &'static str {
    match checks {
        Some(state) => state.as_str(),
        None => "none",
    }
}

/// Everything after `cursor`.
///
/// Events are appended in sequence order, so this is a suffix. A cursor ahead
/// of the log — a subscriber reading a file that was reset — yields nothing
/// rather than replaying, which is the safer way to be wrong.
pub fn since(events: &[Event], cursor: u64) -> &[Event] {
    let start = events.partition_point(|event| event.seq <= cursor);
    &events[start..]
}

/// Everything after `cursor` that this subscriber is the audience for.
///
/// Watches record who asked for them, so a transition reaches the sessions
/// that were watching it and no others — one poll still serves all of them.
pub fn addressed<'a>(events: &'a [Event], cursor: u64, subscriber: &str) -> Vec<&'a Event> {
    since(events, cursor)
        .iter()
        .filter(|event| event.audience.contains(subscriber))
        .collect()
}

/// Where a reader has got to: which stream, and how far into it.
///
/// One token rather than a bare number, because a position means nothing
/// apart from the stream it belongs to. A number alone can be presented
/// against anybody's stream, which in a machine with several sessions running
/// is how one session ends up reading another's news.
///
/// Opaque by contract. The `<owner>:<seq>` spelling is what it happens to be
/// today; callers round-trip the token `inkling events` printed rather than
/// building one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub owner: String,
    pub seq: u64,
}

impl Cursor {
    pub fn new(owner: impl Into<String>, seq: u64) -> Self {
        Self {
            owner: owner.into(),
            seq,
        }
    }
}

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.owner, self.seq)
    }
}

#[derive(Debug, thiserror::Error)]
#[error(
    "{0}: a cursor now names the stream it belongs to, not just a position in \
     one — pass the token `inkling events --json` printed, or name the reader \
     with --subscriber <id>"
)]
pub struct UnknownCursor(String);

impl std::str::FromStr for Cursor {
    type Err = UnknownCursor;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        // From the right, so an owner that contains a colon still parses.
        let (owner, seq) = text
            .rsplit_once(':')
            .ok_or_else(|| UnknownCursor(text.to_string()))?;
        let seq = seq.parse().map_err(|_| UnknownCursor(text.to_string()))?;
        match owner.is_empty() {
            true => Err(UnknownCursor(text.to_string())),
            false => Ok(Self::new(owner, seq)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_change_says_who_caused_it() {
        let by = Actors {
            approved: vec!["@alice".into(), "buildsworth (bot)".into()],
            draft: Some("@dpep".into()),
            merged: Some("@carol".into()),
        };
        let of = |was, now| by.of(was, now);
        assert_eq!(
            of(State::Draft, State::Approved),
            Some("by @alice / buildsworth (bot)".into()),
            "who approved, not who readied it on the way"
        );
        assert_eq!(of(State::Draft, State::Merged), Some("by @carol".into()));
        assert_eq!(of(State::Draft, State::Open), Some("by @dpep".into()));
        assert_eq!(of(State::Open, State::Draft), Some("by @dpep".into()));
        assert_eq!(
            of(State::Approved, State::Open),
            None,
            "an old toggle says nothing about a withdrawn approval"
        );
        assert_eq!(Actors::default().of(State::Open, State::Merged), None);
    }

    fn pull(state: State, checks: Option<CheckState>) -> Sighting {
        Sighting {
            standing: Standing::PullRequest { state, checks },
            title: "Add inkling wait".into(),
            failing: None,
            head: None,
            by: Default::default(),
        }
    }

    fn issue(status: &str, category: Category) -> Sighting {
        Sighting {
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

    /// What moved, where the standing that came back with it isn't the point.
    fn changed(before: Option<&Standing>, after: &Sighting) -> Vec<Transition> {
        reading(before, after).moved
    }

    fn labels(moved: &[Transition]) -> Vec<(&str, &str, &str)> {
        moved
            .iter()
            .map(|t| (t.axis.as_str(), t.from.as_str(), t.to.as_str()))
            .collect()
    }

    fn checks(state: State, checks: Option<CheckState>) -> Standing {
        Standing::PullRequest { state, checks }
    }

    #[test]
    fn the_first_sighting_is_a_baseline_not_an_event() {
        // The rule the whole feed rests on: a subscriber hears about change,
        // not about where things already were when it subscribed.
        assert!(changed(None, &pull(State::Open, None)).is_empty());
        assert!(changed(None, &pull(State::Merged, Some(CheckState::Failure))).is_empty());
        assert!(changed(None, &issue("In Progress", Category::Indeterminate)).is_empty());
    }

    #[test]
    fn a_lifecycle_move_is_one_event() {
        let before = Standing::PullRequest {
            state: State::Open,
            checks: None,
        };
        let moved = changed(Some(&before), &pull(State::Merged, None));
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].axis, Axis::State);
        assert_eq!(
            (moved[0].from.as_str(), moved[0].to.as_str()),
            ("open", "merged")
        );
    }

    #[test]
    fn standing_still_is_not_an_event() {
        let before = Standing::PullRequest {
            state: State::Open,
            checks: Some(CheckState::Pending),
        };
        assert!(changed(Some(&before), &pull(State::Open, Some(CheckState::Pending))).is_empty());
    }

    #[test]
    fn checks_and_lifecycle_move_independently() {
        // Both in one poll is two events, not one merged summary: a script
        // branching on "checks went red" shouldn't have to parse a compound.
        let before = Standing::PullRequest {
            state: State::Open,
            checks: Some(CheckState::Pending),
        };
        let moved = changed(
            Some(&before),
            &pull(State::Approved, Some(CheckState::Success)),
        );
        assert_eq!(
            moved.iter().map(|t| t.axis).collect::<Vec<_>>(),
            vec![Axis::State, Axis::Checks]
        );
    }

    #[test]
    fn a_red_suite_carries_the_job_that_failed() {
        let before = Standing::PullRequest {
            state: State::Open,
            checks: Some(CheckState::Pending),
        };
        let after = Sighting {
            failing: Some(FailedCheck {
                name: "test / build".into(),
                job: Some(7),
            }),
            ..pull(State::Open, Some(CheckState::Failure))
        };
        let moved = changed(Some(&before), &after);
        assert_eq!(moved[0].detail.as_deref(), Some("test / build"));
        assert_eq!(moved[0].job, Some(7));
    }

    #[test]
    fn which_job_is_red_is_not_itself_a_transition() {
        // Rollup unchanged at failure, a different job named: the news has
        // already been delivered, and repeating it every poll is a loop.
        let before = Standing::PullRequest {
            state: State::Open,
            checks: Some(CheckState::Failure),
        };
        let after = Sighting {
            failing: Some(FailedCheck {
                name: "lint".into(),
                job: None,
            }),
            ..pull(State::Open, Some(CheckState::Failure))
        };
        assert!(changed(Some(&before), &after).is_empty());
    }

    #[test]
    fn a_commit_with_no_rollup_yet_keeps_the_one_it_had() {
        // Found by dogfooding: pushing to a pull request produced `checks
        // success → none` and `none → pending` before the `pending → success`
        // that was the news. The head commit briefly has no rollup, which says
        // nothing about where CI stands.
        let green = checks(State::Open, Some(CheckState::Success));
        let gap = reading(Some(&green), &pull(State::Open, None));
        assert!(gap.moved.is_empty(), "{:?}", gap.moved);
        assert_eq!(gap.standing, green, "the rollup is still the one we saw");

        // And when the new suite turns up, the news is that it started — the
        // same pair of events a poll that missed the gap would have reported.
        let started = reading(
            Some(&gap.standing),
            &pull(State::Open, Some(CheckState::Pending)),
        );
        assert_eq!(labels(&started.moved), [("checks", "success", "pending")]);
    }

    #[test]
    fn a_verdict_reached_while_the_rollup_was_missing_still_arrives() {
        // Why the gap is carried over rather than skipped over: suppressing
        // transitions in *both* directions would let a poll that lands in the
        // gap swallow a red suite entirely.
        let green = checks(State::Open, Some(CheckState::Success));
        let gap = reading(Some(&green), &pull(State::Open, None));
        let red = reading(
            Some(&gap.standing),
            &pull(State::Open, Some(CheckState::Failure)),
        );
        assert_eq!(labels(&red.moved), [("checks", "success", "failure")]);
    }

    #[test]
    fn a_pull_request_with_no_rollup_to_carry_reports_the_first_one() {
        // Nothing was ever seen, so there is nothing to carry and the first
        // rollup is a real first observation. The asymmetry is the point: a
        // suite that goes straight to red on a pull request watched before its
        // first check existed must not be silent.
        let none = checks(State::Open, None);
        let baseline = reading(None, &pull(State::Open, None));
        assert!(baseline.moved.is_empty());
        assert_eq!(
            baseline.standing, none,
            "no checks is a standing of its own"
        );

        let first = reading(Some(&none), &pull(State::Open, Some(CheckState::Failure)));
        assert_eq!(labels(&first.moved), [("checks", "none", "failure")]);
    }

    #[test]
    fn a_suite_going_back_to_pending_is_news() {
        // Check terminality isn't permanent — a re-run un-settles it — so the
        // watch must keep reporting rather than stop at the first verdict.
        let before = Standing::PullRequest {
            state: State::Open,
            checks: Some(CheckState::Failure),
        };
        let moved = changed(Some(&before), &pull(State::Open, Some(CheckState::Pending)));
        assert_eq!(
            (moved[0].from.as_str(), moved[0].to.as_str()),
            ("failure", "pending")
        );
    }

    #[test]
    fn an_issue_reports_its_status_name_not_its_category() {
        // Two statuses in the same category is the common Jira move — To Do
        // to In Progress is both `new` and `indeterminate`, but "In Review"
        // to "In QA" is neither, and both are what the watcher asked about.
        let before = Standing::Issue {
            status: "In Review".into(),
            category: Category::Indeterminate,
        };
        let moved = changed(Some(&before), &issue("In QA", Category::Indeterminate));
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].axis, Axis::Status);
        assert_eq!(
            (moved[0].from.as_str(), moved[0].to.as_str()),
            ("In Review", "In QA")
        );
    }

    #[test]
    fn only_the_lifecycle_finishes_a_watch() {
        let red = Standing::PullRequest {
            state: State::Open,
            checks: Some(CheckState::Failure),
        };
        assert!(!red.is_finished(), "a re-run can still turn this green");

        let merged = Standing::PullRequest {
            state: State::Merged,
            checks: Some(CheckState::Pending),
        };
        assert!(merged.is_finished(), "nothing follows merged");
    }

    #[test]
    fn a_done_issue_finishes_its_watch() {
        for (status, category, finished) in [
            ("To Do", Category::New, false),
            ("In Progress", Category::Indeterminate, false),
            ("Won't Do", Category::Done, true),
        ] {
            let standing = Standing::Issue {
                status: status.into(),
                category,
            };
            assert_eq!(standing.is_finished(), finished, "{status}");
        }
    }

    fn log(seqs: &[u64]) -> Vec<Event> {
        seqs.iter()
            .map(|seq| Event {
                seq: *seq,
                at: "2026-09-20T12:00:00Z".parse().unwrap(),
                reference: Reference::JiraIssue { key: "AI-1".into() },
                axis: Axis::Status,
                from: "To Do".into(),
                to: "Done".into(),
                title: String::new(),
                detail: None,
                job: None,
                audience: BTreeSet::new(),
            })
            .collect()
    }

    fn addressed_to(seq: u64, who: &[&str]) -> Event {
        Event {
            audience: who.iter().map(|w| w.to_string()).collect(),
            ..log(&[seq]).remove(0)
        }
    }

    #[test]
    fn a_transition_reaches_the_sessions_that_were_watching_it() {
        // The reason watches carry owners at all: with several sessions
        // running at once, session A's CI result landing in session B's turn
        // is noise that B cannot act on.
        let events = vec![addressed_to(1, &["a"]), addressed_to(2, &["a", "b"])];

        let seqs =
            |who: &str| -> Vec<u64> { addressed(&events, 0, who).iter().map(|e| e.seq).collect() };
        assert_eq!(seqs("a"), vec![1, 2]);
        assert_eq!(seqs("b"), vec![2], "b started watching after the first");
        assert!(seqs("c").is_empty());
    }

    #[test]
    fn a_cursor_round_trips_through_the_form_it_is_printed_in() {
        let cursor: Cursor = "session-a:7".parse().unwrap();
        assert_eq!(cursor, Cursor::new("session-a", 7));
        assert_eq!(cursor.to_string(), "session-a:7");
    }

    #[test]
    fn a_bare_number_is_refused_rather_than_read_as_somebody_s_stream() {
        // What a caller written against the first release would pass. Reading
        // it as "position 5 of whoever is asking" is how one session ends up
        // with another session's news.
        let error = "5".parse::<Cursor>().unwrap_err();
        assert!(error.to_string().contains("names the stream"), "{error}");
        assert!(":5".parse::<Cursor>().is_err(), "nor an empty owner");
    }

    #[test]
    fn an_audience_does_not_change_where_a_cursor_starts() {
        // Filtering happens after the suffix, so a subscriber never sees
        // anything from before its cursor however it was addressed.
        let events = vec![addressed_to(1, &["a"]), addressed_to(2, &["a"])];
        assert_eq!(
            addressed(&events, 1, "a")
                .iter()
                .map(|e| e.seq)
                .collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn a_cursor_returns_what_comes_after_it() {
        let events = log(&[1, 2, 3]);
        assert_eq!(
            since(&events, 0).iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            since(&events, 2).iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![3]
        );
        assert!(since(&events, 3).is_empty(), "caught up means nothing");
    }

    #[test]
    fn a_cursor_ahead_of_the_log_replays_nothing() {
        // A reset file must not hand a subscriber somebody else's history.
        assert!(since(&log(&[1, 2]), 99).is_empty());
    }

    #[test]
    fn a_pruned_prefix_does_not_resurrect() {
        // Old events aged out; a cursor pointing into the gap still only ever
        // moves forward.
        assert_eq!(
            since(&log(&[7, 8]), 3)
                .iter()
                .map(|e| e.seq)
                .collect::<Vec<_>>(),
            vec![7, 8]
        );
    }
}
