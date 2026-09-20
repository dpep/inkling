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
        /// `None` when the repository runs no checks at all.
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

/// One reading of one reference, as a poll hands it over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sighting {
    pub standing: Standing,
    /// What the thing is called, so an event reads without a second lookup.
    pub title: String,
    /// Which check is red, when one is and the provider could name it.
    ///
    /// Deliberately outside `Standing`: a different job failing is the same
    /// news, and including it in the comparison would emit an event every
    /// time the first red job changed name.
    pub failing: Option<String>,
}

/// A change worth telling somebody about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub axis: Axis,
    pub from: String,
    pub to: String,
    /// Extra the axis alone doesn't carry — the name of the failing check.
    pub detail: Option<String>,
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
}

/// What changed between the last standing and this sighting.
///
/// `before` is `None` on the first sighting of a watch, which yields nothing:
/// a baseline is not an event. Without that rule every new watch would
/// announce a state its owner had just looked up.
pub fn transitions(before: Option<&Standing>, after: &Sighting) -> Vec<Transition> {
    let Some(before) = before else {
        return Vec::new();
    };

    match (before, &after.standing) {
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
                    detail: None,
                });
            }
            if had != has {
                moved.push(Transition {
                    axis: Axis::Checks,
                    from: checks_label(*had).into(),
                    to: checks_label(*has).into(),
                    detail: after.failing.clone(),
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
            }]
        }
        // Same status, or a reference that somehow changed kind — which it
        // can't, since a watch is keyed by the reference itself.
        _ => Vec::new(),
    }
}

/// `None` is a repository with no checks, which reads better as a word than
/// as an absence in `pending → none`.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pull(state: State, checks: Option<CheckState>) -> Sighting {
        Sighting {
            standing: Standing::PullRequest { state, checks },
            title: "Add inkling wait".into(),
            failing: None,
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
        }
    }

    #[test]
    fn the_first_sighting_is_a_baseline_not_an_event() {
        // The rule the whole feed rests on: a subscriber hears about change,
        // not about where things already were when it subscribed.
        assert!(transitions(None, &pull(State::Open, None)).is_empty());
        assert!(transitions(None, &pull(State::Merged, Some(CheckState::Failure))).is_empty());
        assert!(transitions(None, &issue("In Progress", Category::Indeterminate)).is_empty());
    }

    #[test]
    fn a_lifecycle_move_is_one_event() {
        let before = Standing::PullRequest {
            state: State::Open,
            checks: None,
        };
        let moved = transitions(Some(&before), &pull(State::Merged, None));
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
        assert!(
            transitions(Some(&before), &pull(State::Open, Some(CheckState::Pending))).is_empty()
        );
    }

    #[test]
    fn checks_and_lifecycle_move_independently() {
        // Both in one poll is two events, not one merged summary: a script
        // branching on "checks went red" shouldn't have to parse a compound.
        let before = Standing::PullRequest {
            state: State::Open,
            checks: Some(CheckState::Pending),
        };
        let moved = transitions(
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
            failing: Some("test / build".into()),
            ..pull(State::Open, Some(CheckState::Failure))
        };
        let moved = transitions(Some(&before), &after);
        assert_eq!(moved[0].detail.as_deref(), Some("test / build"));
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
            failing: Some("lint".into()),
            ..pull(State::Open, Some(CheckState::Failure))
        };
        assert!(transitions(Some(&before), &after).is_empty());
    }

    #[test]
    fn a_suite_going_back_to_pending_is_news() {
        // Check terminality isn't permanent — a re-run un-settles it — so the
        // watch must keep reporting rather than stop at the first verdict.
        let before = Standing::PullRequest {
            state: State::Open,
            checks: Some(CheckState::Failure),
        };
        let moved = transitions(Some(&before), &pull(State::Open, Some(CheckState::Pending)));
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
        let moved = transitions(Some(&before), &issue("In QA", Category::Indeterminate));
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
            })
            .collect()
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
