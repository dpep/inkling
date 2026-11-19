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
use std::collections::{BTreeMap, BTreeSet};
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
        /// `None` until a reading has counted them, which is also what a
        /// watch stored before comments were read has: a baseline, not news.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        comments: Option<Comments>,
    },
    Issue {
        status: String,
        category: Category,
    },
    /// A Slack message: its reactions, as emoji → how many, and the replies
    /// after it in its thread, as ts → who wrote it. Counts rather than names
    /// for reactions because that is all the bridge can read; reply authors
    /// because "who replied" is the news, and the ts says which are new.
    Message {
        #[serde(default)]
        reactions: BTreeMap<String, u32>,
        #[serde(default)]
        replies: BTreeMap<String, String>,
    },
    /// A Slack channel: the newest top-level message read so far, and what
    /// the last read found after the one before it, as ts → who wrote it.
    /// `more` is a read that hit its page limit, so `arrived` is a floor.
    /// `count` is how many have arrived since the watch began: the running
    /// total every accumulating axis keeps, never said as such.
    Channel {
        #[serde(default)]
        count: u64,
        #[serde(default)]
        newest: Option<String>,
        #[serde(default)]
        arrived: BTreeMap<String, String>,
        #[serde(default)]
        more: bool,
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
            // A conversation never finishes; the watch's deadline ends it.
            Standing::Message { .. } | Standing::Channel { .. } => false,
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
    /// How many replies a Slack message has.
    Replies,
    /// How many of one emoji a Slack message carries.
    Reactions,
    /// How many comments a pull request has: conversation, line, review.
    Comments,
    /// New top-level messages in a Slack channel, from the newest ts seen
    /// before to the newest now; the detail says how many and from whom.
    Messages,
}

impl Axis {
    /// The axis a reference's own standing moves on, which is where its end
    /// (`watched → gone`) is said too.
    pub fn of(reference: &Reference) -> Axis {
        match reference {
            Reference::JiraIssue { .. } => Axis::Status,
            _ => Axis::State,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Axis::State => "state",
            Axis::Checks => "checks",
            Axis::Status => "status",
            Axis::Replies => "replies",
            Axis::Reactions => "reactions",
            Axis::Comments => "comments",
            Axis::Messages => "messages",
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

/// How far into a pull request's conversation the last reading got: enough to
/// tell a new comment from one already counted, and nothing anybody wrote.
///
/// A time and not a set of ids, because a page of the latest comments slides:
/// one deleted pulls an older one into view, and a set would call it new.
/// Comments are only ever added at the end, so "after the newest one counted"
/// is new, and an edit, which keeps its comment's time, is not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comments {
    /// How many, as far as the readings could count. Only ever grows: a
    /// deleted comment is not news.
    pub count: u64,
    /// When the newest comment counted was made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<DateTime<Utc>>,
    /// The comments made at exactly `latest`. GitHub's times are to the
    /// second, so the time alone can't say whether another comment in that
    /// second was counted.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub at_latest: BTreeSet<String>,
}

impl Comments {
    fn baseline(said: &Discussion) -> Self {
        let mut mark = Comments {
            count: said.total,
            latest: None,
            at_latest: BTreeSet::new(),
        };
        mark.advance(&said.remarks);
        mark
    }

    fn is_new(&self, remark: &Remark) -> bool {
        self.latest.is_none_or(|latest| {
            remark.at > latest || (remark.at == latest && !self.at_latest.contains(&remark.id))
        })
    }

    fn advance<'a>(&mut self, remarks: impl IntoIterator<Item = &'a Remark>) {
        for remark in remarks {
            match self.latest {
                Some(latest) if remark.at < latest => {}
                Some(latest) if remark.at == latest => {
                    self.at_latest.insert(remark.id.clone());
                }
                _ => {
                    self.latest = Some(remark.at);
                    self.at_latest = BTreeSet::from([remark.id.clone()]);
                }
            }
        }
    }
}

/// Something said on a pull request — a conversation comment, or a submitted
/// review with its line comments — as a reading keeps it: who and when, never
/// what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remark {
    pub id: String,
    pub at: DateTime<Utc>,
    /// `@login`, or `login (bot)`.
    pub by: String,
    /// A review's line comments, published with it. Kept apart because
    /// `gh pr view --comments` doesn't show them.
    pub lines: u64,
    /// Whether it says something beyond its line comments: a conversation
    /// comment always does, a review when it has a body.
    pub body: bool,
    /// An approval, whose body the state axis may already have said.
    pub approval: bool,
}

impl Remark {
    /// How many comments it is.
    pub fn weight(&self) -> u64 {
        self.lines + u64::from(self.body)
    }
}

/// What one reading saw of a pull request's conversation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discussion {
    /// The latest of each kind, a page of each.
    pub remarks: Vec<Remark>,
    /// How many comments there are, as far as the pages and GitHub's counts
    /// say: exact unless there are more reviews than a page holds.
    pub total: u64,
    /// Every remark made at or after this is in `remarks`. `None` when all of
    /// them are; otherwise a burst bigger than a page may reach past it.
    pub complete_after: Option<DateTime<Utc>>,
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
    /// A pull request's comments, when the provider read them. What is new
    /// depends on the reading before, so `reading` turns this into the
    /// standing's `Comments`.
    pub discussion: Option<Discussion>,
}

/// Who moved a pull request, as `@login`, or `login (bot)` for an app: a
/// review bot satisfying the requirement, or automation sending a pull
/// request back to draft, is different news from a person. For a Slack
/// message, how to name the people in it; for a pull request, which of them
/// is the reader.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Actors {
    /// Whose approvals stand.
    pub approved: Vec<String>,
    /// Who last marked it ready or converted it to draft.
    pub draft: Option<String>,
    /// Who merged it.
    pub merged: Option<String>,
    /// Slack user id → the name a push says, for whoever a reading met.
    pub names: BTreeMap<String, String>,
    /// The GitHub `@login` the credentials belong to, spelled as a pull
    /// request's actors are. Slack has no equivalent: the Slack MCP posts as
    /// the user, so who wrote a message can't say whether the session did —
    /// see `Receipts`.
    pub me: Option<String>,
}

impl Actors {
    /// Who put a pull request in the state it is now in, if anyone can say.
    /// Only the end state counts: whoever readied a PR on its way to merged
    /// is not who merged it.
    fn of(&self, was: State, now: State) -> Option<String> {
        let who = match now {
            State::Merged => self.merged.clone()?,
            State::Approved if !self.approved.is_empty() => {
                listed(&self.approved.iter().map(String::as_str).collect::<Vec<_>>())
            }
            State::Draft => self.draft.clone()?,
            State::Open if was == State::Draft => self.draft.clone()?,
            _ => return None,
        };
        Some(format!("by {who}"))
    }

    /// `@name` for a Slack user id, or the id when no reading named them.
    fn named(&self, id: &str) -> String {
        match id {
            "" => "someone".to_string(),
            id => format!("@{}", self.names.get(id).map_or(id, String::as_str)),
        }
    }
}

/// Something a session itself put into Slack, as the `post-tool` hook saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Receipt {
    Posted {
        channel: String,
        ts: String,
    },
    Reacted {
        channel: String,
        ts: String,
        emoji: String,
    },
}

/// How many people a list names; past that it counts them.
const NAMED: usize = 3;

/// The one wording for a list of who: `@a`, `@a and b (bot)`, `@a, @b and
/// @c`, and past three a count, `5 people and 2 bots`, since a bot isn't a
/// person and the split is what a reader weighs.
pub fn listed(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] if names.len() <= NAMED => format!("{} and {last}", rest.join(", ")),
        _ => {
            let bots = names.iter().filter(|n| n.ends_with(" (bot)")).count();
            let people = names.len() - bots;
            let counted = |n: usize, one: &str, many: &str| match n {
                1 => format!("1 {one}"),
                n => format!("{n} {many}"),
            };
            match (people, bots) {
                (_, 0) => counted(people, "person", "people"),
                (0, _) => counted(bots, "bot", "bots"),
                _ => format!(
                    "{} and {}",
                    counted(people, "person", "people"),
                    counted(bots, "bot", "bots")
                ),
            }
        }
    }
}

fn is_zero(n: &u64) -> bool {
    *n == 0
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
    /// Who made it, as far as the reading can say. Whether that was a given
    /// reader is settled against its receipts when the event is recorded.
    pub made: Made,
    /// A count whose page didn't reach back to the last reading: at least
    /// this many, maybe more.
    pub partial: bool,
    /// How many of the new comments are line comments, which need a
    /// different command to read than the rest.
    pub lines: u64,
    /// The Slack ts the news came after: a channel read from it shows what
    /// arrived.
    pub after: Option<String>,
}

/// Who made a transition, as far as one reading can say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Made {
    #[default]
    Others,
    /// The account the credentials belong to, which every reader shares.
    Viewer,
    /// These new Slack messages, by ts.
    Posted(BTreeSet<String>),
    /// This many more of one emoji on the watched message.
    Reacted { emoji: String, added: u32 },
}

impl Made {
    /// Whether this was the reader itself, judged by what it is on record as
    /// having done. Spends the reactions it accounts for, so the next one of
    /// the same emoji is somebody else's.
    pub fn by(&self, receipts: Option<&mut Receipts>) -> bool {
        match (self, receipts) {
            (Made::Others, _) => false,
            (Made::Viewer, _) => true,
            (_, None) => false,
            (Made::Posted(ts), Some(mine)) => !ts.is_empty() && ts.is_subset(&mine.posted),
            (Made::Reacted { emoji, added }, Some(mine)) => {
                let had = mine.reacted.iter().filter(|r| r.emoji == *emoji).count();
                let mut spend = had.min(*added as usize);
                mine.reacted.retain(|r| {
                    let spent = spend > 0 && r.emoji == *emoji;
                    spend -= usize::from(spent);
                    !spent
                });
                *added as usize <= had
            }
        }
    }
}

/// What one reader put into a watched Slack conversation itself — the only
/// evidence a message or reaction was the session's, since the Slack MCP
/// posts as the user and a reply from their phone has the same author.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipts {
    /// Messages it posted, by ts. A ts is unique, so these never need
    /// spending; they go when the watch does.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub posted: BTreeSet<String>,
    /// Reactions it added to the watched message, not yet seen by a reading.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reacted: Vec<Reacted>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reacted {
    pub emoji: String,
    pub at: DateTime<Utc>,
}

impl Receipts {
    /// Forget reactions a reading taken at `at` already reflects. One that
    /// moved no count was a duplicate Slack ignored, and must not claim the
    /// next person's.
    pub fn settle(&mut self, at: DateTime<Utc>) {
        self.reacted.retain(|r| r.at > at);
    }

    pub fn is_empty(&self) -> bool {
        self.posted.is_empty() && self.reacted.is_empty()
    }
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
    /// The readers this is a receipt for — what each did itself, recorded
    /// but not worth a turn. Per reader, because two sessions share one Slack
    /// identity and only the one that posted a reply has a receipt for it.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub receipt_for: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub lines: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
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
    let mut standing = carried_over(before, &after.standing);
    let mut moved = before.map_or_else(Vec::new, |before| compared(before, &standing, after));
    if let Standing::PullRequest {
        state: now,
        comments,
        ..
    } = &mut standing
    {
        let (had, was) = match before {
            Some(Standing::PullRequest {
                comments, state, ..
            }) => (comments.as_ref(), Some(*state)),
            _ => (None, None),
        };
        // An approval the state axis is about to name is one action, so it
        // is said once, there.
        let approved = *now == State::Approved && was.is_some_and(|was| was != State::Approved);
        let (mark, said) = commented(had, after.discussion.as_ref(), approved, &after.by);
        *comments = mark;
        moved.extend(said);
    }
    if let (
        Standing::Channel {
            count,
            arrived,
            more,
            ..
        },
        Some(Standing::Channel {
            count: had,
            newest: was,
            ..
        }),
    ) = (&mut standing, before)
    {
        let (total, said) = arrivals(was.as_deref(), *had, arrived, *more, &after.by);
        *count = total;
        moved.extend(said);
    }
    Reading { standing, moved }
}

/// What moved between two standings, on every axis but comments, which need
/// the reading's discussion as well as the standing.
fn compared(before: &Standing, standing: &Standing, after: &Sighting) -> Vec<Transition> {
    match (before, standing) {
        (
            Standing::PullRequest {
                state: was,
                checks: had,
                ..
            },
            Standing::PullRequest {
                state: now,
                checks: has,
                ..
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
                    made: Made::Others,
                    partial: false,
                    lines: 0,
                    after: None,
                });
            }
            if had != has {
                moved.push(Transition {
                    axis: Axis::Checks,
                    from: checks_label(*had).into(),
                    to: checks_label(*has).into(),
                    detail: after.failing.as_ref().map(|f| f.name.clone()),
                    job: after.failing.as_ref().and_then(|f| f.job),
                    made: Made::Others,
                    partial: false,
                    lines: 0,
                    after: None,
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
                made: Made::Others,
                partial: false,
                lines: 0,
                after: None,
            }]
        }
        (
            Standing::Message {
                reactions: had,
                replies: were,
            },
            Standing::Message {
                reactions: has,
                replies: are,
            },
        ) => conversation(had, were, has, are, &after.by),
        // Same status, or a reference that somehow changed kind — which it
        // can't, since a watch is keyed by the reference itself.
        _ => Vec::new(),
    }
}

/// Where a pull request's comments stand after this reading, and the
/// transition when somebody said something new.
///
/// Only growth is news. An edit keeps its comment's time, so a coverage bot
/// rewriting its comment on every push is not new; a deleted comment is not
/// news either, so the count never goes down. A reading that didn't count
/// them keeps the mark it had, and the first one that does is a baseline.
fn commented(
    had: Option<&Comments>,
    said: Option<&Discussion>,
    approved: bool,
    by: &Actors,
) -> (Option<Comments>, Option<Transition>) {
    let (had, said) = match (had, said) {
        (had, None) => return (had.cloned(), None),
        (None, Some(said)) => return (Some(Comments::baseline(said)), None),
        (Some(had), Some(said)) => (had, said),
    };
    let mut new: Vec<&Remark> = said.remarks.iter().filter(|r| had.is_new(r)).collect();
    new.sort_by_key(|remark| remark.at);
    // The oldest of a full page is itself new, so the burst may reach further
    // back than the page did. GitHub's totals make up the number, but not for
    // a comment deleted meanwhile, so it is a floor.
    let partial = said
        .complete_after
        .is_some_and(|after| had.latest.is_none_or(|latest| after > latest));
    let mut mark = had.clone();
    mark.advance(new.iter().copied());
    mark.count = mark
        .count
        .saturating_add(new.iter().map(|r| r.weight()).sum());
    // Only a burst past the page needs the total: otherwise the page held
    // everything new, and the total also grows by a body edited into a review.
    if partial {
        mark.count = mark.count.max(said.total);
    }

    // What the state axis said is still counted, so it starts the line's
    // count rather than being part of it.
    let (mut unsaid, mut lines) = (0, 0);
    let mut names: Vec<&str> = Vec::new();
    for remark in &new {
        let told = match approved && remark.approval {
            true => remark.lines,
            false => remark.weight(),
        };
        unsaid += remark.weight() - told;
        if told == 0 {
            continue;
        }
        lines += remark.lines;
        if !names.contains(&remark.by.as_str()) {
            names.push(&remark.by);
        }
    }
    if names.is_empty() {
        return (Some(mark), None);
    }
    let made = match by.me.as_deref() {
        Some(me) if names.iter().all(|name| *name == me) => Made::Viewer,
        _ => Made::Others,
    };
    let moved = Transition {
        axis: Axis::Comments,
        from: (had.count + unsaid).to_string(),
        to: mark.count.to_string(),
        detail: Some(names.join(", ")),
        job: None,
        made,
        partial,
        lines,
        after: None,
    };
    (Some(mark), Some(moved))
}

/// The transition for the replies in `new`, out of all a Slack message has
/// (`are`), naming who wrote them. `new` may be fewer than arrived: a reply is
/// news on one watched message per reader (`Watches::record`), so the count
/// runs up to every reply and the step is the ones told here.
pub fn replied(
    are: &BTreeMap<String, String>,
    new: &BTreeSet<String>,
    by: &Actors,
) -> Option<Transition> {
    if new.is_empty() {
        return None;
    }
    let authors: BTreeSet<&String> = new.iter().filter_map(|ts| are.get(ts)).collect();
    let names: Vec<String> = authors.iter().map(|id| by.named(id)).collect();
    Some(Transition {
        axis: Axis::Replies,
        from: are.len().saturating_sub(new.len()).to_string(),
        to: are.len().to_string(),
        detail: (!names.is_empty()).then(|| names.join(", ")),
        job: None,
        made: posted(new),
        partial: false,
        lines: 0,
        after: None,
    })
}

/// What moved on a Slack message: one transition for its replies, naming who
/// wrote the new ones, and one per emoji whose count changed.
///
/// Nobody is named for a reaction, because the bridge reads a count and not
/// who reacted.
fn conversation(
    had: &BTreeMap<String, u32>,
    were: &BTreeMap<String, String>,
    has: &BTreeMap<String, u32>,
    are: &BTreeMap<String, String>,
    by: &Actors,
) -> Vec<Transition> {
    let new: BTreeSet<String> = are
        .keys()
        .filter(|ts| !were.contains_key(*ts))
        .cloned()
        .collect();
    let mut moved: Vec<Transition> = replied(are, &new, by).into_iter().collect();
    let emoji: BTreeSet<&String> = had.keys().chain(has.keys()).collect();
    for name in emoji {
        let (was, now) = (
            had.get(name).copied().unwrap_or(0),
            has.get(name).copied().unwrap_or(0),
        );
        if was != now {
            moved.push(Transition {
                axis: Axis::Reactions,
                from: was.to_string(),
                to: now.to_string(),
                detail: Some(format!(":{name}:")),
                job: None,
                made: match now > was {
                    true => Made::Reacted {
                        emoji: name.clone(),
                        added: now - was,
                    },
                    false => Made::Others,
                },
                partial: false,
                lines: 0,
                after: None,
            });
        }
    }
    moved
}

/// New Slack messages, which a reader made if it posted every one of them.
fn posted<'a>(ts: impl IntoIterator<Item = &'a String>) -> Made {
    let ts: BTreeSet<String> = ts.into_iter().cloned().collect();
    match ts.is_empty() {
        true => Made::Others,
        false => Made::Posted(ts),
    }
}

/// What arrived in a channel since its newest message before, and the
/// running count after it: one transition naming who posted, never what.
fn arrivals(
    was: Option<&str>,
    had: u64,
    arrived: &BTreeMap<String, String>,
    more: bool,
    by: &Actors,
) -> (u64, Option<Transition>) {
    let new: Vec<(&String, &String)> = arrived
        .iter()
        .filter(|(ts, _)| was.is_none_or(|was| crate::domain::slack::later(ts, was)))
        .collect();
    if new.is_empty() {
        return (had, None);
    }
    let total = had + new.len() as u64;
    let mut authors: Vec<&String> = Vec::new();
    for (_, who) in &new {
        if !authors.contains(who) {
            authors.push(who);
        }
    }
    let names: Vec<String> = authors.iter().map(|id| by.named(id)).collect();
    let moved = Transition {
        axis: Axis::Messages,
        from: had.to_string(),
        to: total.to_string(),
        detail: Some(names.join(", ")),
        job: None,
        made: posted(new.into_iter().map(|(ts, _)| ts)),
        partial: more,
        lines: 0,
        after: was.map(String::from),
    };
    (total, Some(moved))
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
                comments,
            },
        ) => Standing::PullRequest {
            state: *state,
            checks: Some(*had),
            comments: comments.clone(),
        },
        // A reply once seen stays counted, like a comment: a read that left one
        // out — a dropped row, a sliding page — would otherwise announce it
        // again when it turned up.
        (
            Some(Standing::Message { replies: were, .. }),
            Standing::Message { reactions, replies },
        ) => Standing::Message {
            reactions: reactions.clone(),
            replies: were
                .iter()
                .chain(replies)
                .map(|(ts, who)| (ts.clone(), who.clone()))
                .collect(),
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
    fn a_jira_issue_stands_on_its_status_and_the_rest_on_their_state() {
        let jira = Reference::JiraIssue { key: "AI-7".into() };
        assert_eq!(Axis::of(&jira), Axis::Status);
        let channel = Reference::SlackChannel {
            channel: "C1".into(),
            name: None,
        };
        assert_eq!(Axis::of(&channel), Axis::State);
    }

    #[test]
    fn a_state_change_says_who_caused_it() {
        let by = Actors {
            approved: vec!["@alice".into(), "buildsworth (bot)".into()],
            draft: Some("@dpep".into()),
            merged: Some("@carol".into()),
            ..Actors::default()
        };
        let of = |was, now| by.of(was, now);
        assert_eq!(
            of(State::Draft, State::Approved),
            Some("by @alice and buildsworth (bot)".into()),
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

    fn issue(status: &str, category: Category) -> Sighting {
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
        Standing::PullRequest {
            state,
            checks,
            comments: None,
        }
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
            comments: None,
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
            comments: None,
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
            comments: None,
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
            comments: None,
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
            comments: None,
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
        // Regression: pushing to a pull request produced `checks
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
            comments: None,
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
            comments: None,
        };
        assert!(!red.is_finished(), "a re-run can still turn this green");

        let merged = Standing::PullRequest {
            state: State::Merged,
            checks: Some(CheckState::Pending),
            comments: None,
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

    fn channel(count: u64, newest: Option<&str>, arrived: &[(&str, &str)], more: bool) -> Standing {
        Standing::Channel {
            count,
            newest: newest.map(Into::into),
            arrived: arrived
                .iter()
                .map(|(ts, who)| (ts.to_string(), who.to_string()))
                .collect(),
            more,
        }
    }

    #[test]
    fn new_channel_messages_count_up_and_name_who_posted() {
        let before = channel(4, Some("1.000001"), &[], false);
        let after = seen_message(channel(
            0,
            Some("1.000004"),
            &[("1.000002", "U1"), ("1.000003", "U2"), ("1.000004", "U1")],
            false,
        ));
        let read = reading(Some(&before), &after);
        assert_eq!(labels(&read.moved), [("messages", "4", "7")]);
        assert_eq!(read.moved[0].detail.as_deref(), Some("@alice, @bob"));
        assert!(!read.moved[0].partial);
        assert_eq!(
            read.moved[0].after.as_deref(),
            Some("1.000001"),
            "where a read that shows them starts"
        );
        assert!(
            matches!(read.standing, Standing::Channel { count: 7, .. }),
            "the running count is kept: {:?}",
            read.standing
        );
        assert!(!read.standing.is_finished(), "lives until its TTL");
    }

    #[test]
    fn a_quiet_channel_or_a_baseline_says_nothing() {
        let before = channel(1, Some("1.000004"), &[("1.000004", "U1")], false);
        let quiet = seen_message(channel(0, Some("1.000004"), &[], false));
        let read = reading(Some(&before), &quiet);
        assert!(read.moved.is_empty());
        assert!(matches!(read.standing, Standing::Channel { count: 1, .. }));
        let first = seen_message(channel(0, Some("1.000004"), &[("1.000004", "U1")], false));
        assert!(changed(None, &first).is_empty());
    }

    #[test]
    fn a_full_page_is_a_floor() {
        let before = channel(0, None, &[], false);
        let after = seen_message(channel(
            0,
            Some("1.000002"),
            &[("1.000001", "U1"), ("1.000002", "U2")],
            true,
        ));
        let moved = changed(Some(&before), &after);
        assert_eq!(labels(&moved), [("messages", "0", "2")]);
        assert!(moved[0].partial);
    }

    #[test]
    fn channel_messages_are_made_by_whoever_posted_them() {
        let before = channel(0, Some("1.000001"), &[], false);
        let after = seen_message(channel(0, Some("1.000002"), &[("1.000002", "U9")], false));
        let moved = changed(Some(&before), &after);
        assert_eq!(moved[0].detail.as_deref(), Some("@carol"));
        assert_eq!(moved[0].made, Made::Posted(ts(&["1.000002"])));
    }

    fn ts(all: &[&str]) -> BTreeSet<String> {
        all.iter().map(|ts| ts.to_string()).collect()
    }

    fn message(reactions: &[(&str, u32)], replies: &[(&str, &str)]) -> Standing {
        Standing::Message {
            reactions: reactions.iter().map(|(e, n)| (e.to_string(), *n)).collect(),
            replies: replies
                .iter()
                .map(|(ts, who)| (ts.to_string(), who.to_string()))
                .collect(),
        }
    }

    fn seen_message(standing: Standing) -> Sighting {
        Sighting {
            discussion: None,
            standing,
            title: String::new(),
            failing: None,
            head: None,
            by: Actors {
                names: [("U1", "alice"), ("U2", "bob"), ("U9", "carol")]
                    .iter()
                    .map(|(id, name)| (id.to_string(), name.to_string()))
                    .collect(),
                ..Actors::default()
            },
        }
    }

    #[test]
    fn new_replies_say_how_many_and_who_wrote_them() {
        let before = message(&[], &[("1.000001", "U1"), ("1.000002", "U9")]);
        let after = seen_message(message(
            &[],
            &[
                ("1.000001", "U1"),
                ("1.000002", "U9"),
                ("1.000003", "U2"),
                ("1.000004", "U1"),
            ],
        ));
        let moved = changed(Some(&before), &after);
        assert_eq!(labels(&moved), [("replies", "2", "4")]);
        assert_eq!(moved[0].detail.as_deref(), Some("@alice, @bob"));
        assert_eq!(moved[0].made, Made::Posted(ts(&["1.000003", "1.000004"])));
    }

    #[test]
    fn a_reply_with_no_author_is_from_someone_not_a_bare_at() {
        let before = message(&[], &[]);
        let after = seen_message(message(&[], &[("1.000001", ""), ("1.000002", "U1")]));
        let moved = changed(Some(&before), &after);
        assert_eq!(moved[0].detail.as_deref(), Some("someone, @alice"));
    }

    #[test]
    fn a_reader_made_only_messages_it_posted_every_one_of() {
        let mut mine = Receipts {
            posted: ts(&["1.000001"]),
            ..Receipts::default()
        };
        assert!(Made::Posted(ts(&["1.000001"])).by(Some(&mut mine)));
        assert!(
            !Made::Posted(ts(&["1.000001", "1.000002"])).by(Some(&mut mine)),
            "anything it didn't post makes it news"
        );
        assert!(!Made::Posted(ts(&["1.000001"])).by(None), "posted nothing");
        assert!(Made::Viewer.by(None), "the account every reader shares");
    }

    #[test]
    fn a_reaction_receipt_is_spent_once_and_settled_by_the_next_reading() {
        let at = |m: i64| {
            "2026-11-17T12:00:00Z".parse::<DateTime<Utc>>().unwrap() + chrono::TimeDelta::minutes(m)
        };
        let eyes = |added| Made::Reacted {
            emoji: "eyes".into(),
            added,
        };
        let added = |m| Reacted {
            emoji: "eyes".into(),
            at: at(m),
        };
        let mut mine = Receipts {
            reacted: vec![added(0)],
            ..Receipts::default()
        };
        assert!(!eyes(2).by(Some(&mut mine)), "somebody else added one too");
        assert!(mine.reacted.is_empty(), "but it spent the reader's");

        // Slack ignores a duplicate, so a receipt that moved no count is
        // gone once a reading after it has been taken.
        mine.reacted.push(added(1));
        mine.settle(at(0));
        assert_eq!(mine.reacted.len(), 1, "a reading from before it");
        mine.settle(at(2));
        assert!(mine.reacted.is_empty());
    }

    #[test]
    fn each_emoji_that_moved_is_its_own_transition_and_names_nobody() {
        let before = message(&[("eyes", 1), ("tada", 2)], &[]);
        let after = seen_message(message(&[("tada", 2), ("white_check_mark", 1)], &[]));
        let moved = changed(Some(&before), &after);
        assert_eq!(
            labels(&moved),
            [("reactions", "1", "0"), ("reactions", "0", "1")]
        );
        assert_eq!(moved[0].detail.as_deref(), Some(":eyes:"));
        assert_eq!(moved[1].detail.as_deref(), Some(":white_check_mark:"));
        assert_eq!(
            moved[0].made,
            Made::Others,
            "a removal is nobody's to claim"
        );
        assert_eq!(
            moved[1].made,
            Made::Reacted {
                emoji: "white_check_mark".into(),
                added: 1
            }
        );
    }

    #[test]
    fn a_reply_a_read_left_out_is_not_news_when_it_turns_up_again() {
        // A model dropping a row, or a DM's page sliding, must not announce
        // an old reply a second time.
        let both = message(&[], &[("1.000001", "U1"), ("1.000002", "U2")]);
        let dropped = reading(
            Some(&both),
            &seen_message(message(&[], &[("1.000001", "U1")])),
        );
        assert!(dropped.moved.is_empty(), "{:?}", dropped.moved);
        let back = seen_message(message(&[], &[("1.000001", "U1"), ("1.000002", "U2")]));
        assert!(changed(Some(&dropped.standing), &back).is_empty());
    }

    #[test]
    fn a_quiet_message_is_not_an_event_and_never_finishes() {
        let standing = message(&[("eyes", 1)], &[("1.000001", "U1")]);
        assert!(changed(Some(&standing), &seen_message(standing.clone())).is_empty());
        assert!(!standing.is_finished());
    }

    fn remark(id: &str, at: &str, by: &str) -> Remark {
        Remark {
            id: id.into(),
            at: format!("2026-11-17T{at}Z").parse().unwrap(),
            by: by.into(),
            lines: 0,
            body: true,
            approval: false,
        }
    }

    /// A reading of an open pull request by `@dpep`, with these comments.
    fn commenting(state: State, remarks: &[Remark], total: u64) -> Sighting {
        Sighting {
            discussion: Some(Discussion {
                remarks: remarks.to_vec(),
                total,
                ..Discussion::default()
            }),
            by: Actors {
                me: Some("@dpep".into()),
                ..Actors::default()
            },
            ..pull(state, None)
        }
    }

    fn count(standing: &Standing) -> Option<u64> {
        match standing {
            Standing::PullRequest { comments, .. } => comments.as_ref().map(|c| c.count),
            _ => None,
        }
    }

    #[test]
    fn new_comments_say_how_many_and_who_wrote_them() {
        let alice = remark("a", "10:00:00", "@alice");
        let base = reading(
            None,
            &commenting(State::Open, std::slice::from_ref(&alice), 1),
        );
        assert!(base.moved.is_empty(), "a baseline");

        let later = [
            alice,
            remark("b", "10:05:00", "coderabbitai (bot)"),
            remark("c", "10:06:00", "@alice"),
        ];
        let moved = changed(Some(&base.standing), &commenting(State::Open, &later, 3));
        assert_eq!(labels(&moved), [("comments", "1", "3")]);
        assert_eq!(
            moved[0].detail.as_deref(),
            Some("coderabbitai (bot), @alice")
        );
        assert_eq!(moved[0].made, Made::Others);
    }

    #[test]
    fn an_edited_or_deleted_comment_is_not_news() {
        // A coverage bot rewrites its comment in place on every push: same
        // id, same time.
        let (a, b) = (
            remark("a", "10:00:00", "codecov (bot)"),
            remark("b", "10:01:00", "@alice"),
        );
        let base = reading(None, &commenting(State::Open, &[a.clone(), b], 2)).standing;
        let edited = commenting(
            State::Open,
            &[a.clone(), remark("b", "10:01:00", "@alice")],
            2,
        );
        assert!(changed(Some(&base), &edited).is_empty());

        let deleted = reading(
            Some(&base),
            &commenting(State::Open, std::slice::from_ref(&a), 1),
        );
        assert!(deleted.moved.is_empty());
        assert_eq!(count(&deleted.standing), Some(2), "a count never goes back");

        // One deleted and one new: GitHub's total is unchanged, and it is news.
        let c = remark("c", "10:02:00", "@bob");
        let swapped = changed(
            Some(&deleted.standing),
            &commenting(State::Open, &[a, c], 2),
        );
        assert_eq!(labels(&swapped), [("comments", "2", "3")]);
    }

    #[test]
    fn a_comment_in_the_same_second_as_the_last_counted_is_still_new() {
        let a = remark("a", "10:00:00", "@alice");
        let base = reading(None, &commenting(State::Open, std::slice::from_ref(&a), 1)).standing;
        let both = [a, remark("b", "10:00:00", "@bob")];
        let moved = changed(Some(&base), &commenting(State::Open, &both, 2));
        assert_eq!(moved[0].detail.as_deref(), Some("@bob"));
    }

    #[test]
    fn comments_only_the_reader_wrote_are_their_own() {
        let base = reading(None, &commenting(State::Open, &[], 0)).standing;
        let mine = [remark("a", "10:00:00", "@dpep")];
        let moved = changed(Some(&base), &commenting(State::Open, &mine, 1));
        assert_eq!(moved[0].made, Made::Viewer, "recorded, not worth a turn");

        let with_a_bot = [
            mine[0].clone(),
            remark("b", "10:01:00", "coderabbitai (bot)"),
        ];
        assert_eq!(
            changed(Some(&base), &commenting(State::Open, &with_a_bot, 2))[0].made,
            Made::Others
        );
    }

    #[test]
    fn an_approval_the_state_axis_names_is_not_said_twice() {
        let approval = Remark {
            approval: true,
            ..remark("r", "10:00:00", "@alice")
        };
        let base = reading(None, &commenting(State::Open, &[], 0)).standing;

        let approved = reading(
            Some(&base),
            &commenting(State::Approved, std::slice::from_ref(&approval), 1),
        );
        assert_eq!(labels(&approved.moved), [("state", "open", "approved")]);
        assert_eq!(count(&approved.standing), Some(1), "still counted");

        // A second approval required: the state stays open, so its body is
        // only heard as a comment.
        let moved = changed(Some(&base), &commenting(State::Open, &[approval], 1));
        assert_eq!(labels(&moved), [("comments", "0", "1")]);
    }

    #[test]
    fn a_burst_bigger_than_the_page_says_at_least() {
        let base = reading(
            None,
            &commenting(State::Open, &[remark("a", "10:00:00", "@alice")], 1),
        )
        .standing;
        let page: Vec<Remark> = (1..=3)
            .map(|i| remark(&format!("n{i}"), &format!("11:0{i}:00"), "@bob"))
            .collect();
        let burst = Sighting {
            discussion: Some(Discussion {
                remarks: page.clone(),
                total: 41,
                complete_after: Some(page[0].at),
            }),
            ..commenting(State::Open, &[], 0)
        };
        let moved = changed(Some(&base), &burst);
        assert_eq!(labels(&moved), [("comments", "1", "41")]);
        assert_eq!(moved[0].detail.as_deref(), Some("@bob"));
        assert!(
            moved[0].partial,
            "the page didn't reach back to the last one counted"
        );
    }

    fn approving(lines: u64) -> Remark {
        Remark {
            approval: true,
            lines,
            ..remark("r", "10:00:00", "@alice")
        }
    }

    #[test]
    fn a_body_edited_into_an_old_review_is_not_counted_as_the_new_comment() {
        // An edit isn't news, so GitHub's total growing by it mustn't be
        // credited to whoever did write something new.
        let lines_only = Remark {
            lines: 2,
            body: false,
            ..remark("r1", "09:00:00", "@carol")
        };
        let base = reading(
            None,
            &commenting(State::Open, std::slice::from_ref(&lines_only), 2),
        )
        .standing;
        let edited = Remark {
            body: true,
            ..lines_only
        };
        let both = [edited, remark("b", "10:01:00", "@bob")];
        let moved = changed(Some(&base), &commenting(State::Open, &both, 4));
        assert_eq!(labels(&moved), [("comments", "2", "3")], "bob wrote one");
    }

    #[test]
    fn an_approval_beside_another_comment_counts_only_what_is_told() {
        // @alice approves with three line comments; @bob comments once.
        let base = reading(None, &commenting(State::Open, &[], 0)).standing;
        let both = [approving(3), remark("b", "10:01:00", "@bob")];
        let moved = changed(Some(&base), &commenting(State::Approved, &both, 5));
        assert_eq!(
            labels(&moved),
            [("state", "open", "approved"), ("comments", "1", "5")],
            "the approval's body is the state change; four comments are left"
        );
        assert_eq!(moved[1].detail.as_deref(), Some("@alice, @bob"));
        assert_eq!(moved[1].lines, 3, "which command shows them depends on it");
    }

    #[test]
    fn line_comments_left_with_an_approval_get_a_line() {
        let base = reading(None, &commenting(State::Open, &[], 0)).standing;
        let moved = changed(
            Some(&base),
            &commenting(State::Approved, &[approving(3)], 4),
        );
        assert_eq!(
            labels(&moved),
            [("state", "open", "approved"), ("comments", "1", "4")]
        );
        assert_eq!(moved[1].lines, 3);
    }

    #[test]
    fn a_burst_past_the_page_is_at_least_what_github_counted() {
        // Conversation comments past the page: totalCount gives the number,
        // but a comment deleted since the last reading hides one new one, and
        // nothing the page shows can tell. Regression: one of
        // ten deleted, then 31 new, read as exactly 30.
        let base = reading(
            None,
            &commenting(State::Open, &[remark("a", "10:00:00", "@alice")], 1),
        )
        .standing;
        let page: Vec<Remark> = (1..=3)
            .map(|i| remark(&format!("n{i}"), &format!("11:0{i}:00"), "@bob"))
            .collect();
        let burst = Sighting {
            discussion: Some(Discussion {
                remarks: page.clone(),
                total: 31,
                complete_after: Some(page[0].at),
            }),
            ..commenting(State::Open, &[], 0)
        };
        let moved = changed(Some(&base), &burst);
        assert_eq!(labels(&moved), [("comments", "1", "31")]);
        assert!(moved[0].partial, "the count is GitHub's less any deletions");
    }

    #[test]
    fn approvers_are_listed_like_any_other_names() {
        let by = |approved: &[&str]| Actors {
            approved: approved.iter().map(|a| a.to_string()).collect(),
            ..Actors::default()
        };
        assert_eq!(
            by(&["@alice", "review-bot (bot)"]).of(State::Open, State::Approved),
            Some("by @alice and review-bot (bot)".into())
        );
        assert_eq!(
            by(&["@a", "@b", "@c", "x (bot)"]).of(State::Open, State::Approved),
            Some("by 3 people and 1 bot".into())
        );
    }

    #[test]
    fn past_three_names_people_and_bots_are_counted_apart() {
        assert_eq!(listed(&["@a"]), "@a");
        assert_eq!(listed(&["@a", "b (bot)", "@c"]), "@a, b (bot) and @c");
        assert_eq!(
            listed(&["a (bot)", "b (bot)", "c (bot)", "@d"]),
            "1 person and 3 bots"
        );
        assert_eq!(listed(&["@a", "@b", "@c", "@d", "@e"]), "5 people");
        assert_eq!(
            listed(&["a (bot)", "b (bot)", "c (bot)", "d (bot)"]),
            "4 bots"
        );
    }

    #[test]
    fn a_watch_stored_before_comments_were_read_baselines_them() {
        let legacy = checks(State::Open, None);
        let read = reading(
            Some(&legacy),
            &commenting(State::Open, &[remark("a", "10:00:00", "@alice")], 1),
        );
        assert!(read.moved.is_empty(), "{:?}", read.moved);
        assert_eq!(count(&read.standing), Some(1));

        // And a reading that didn't count them keeps the mark.
        let unread = reading(Some(&read.standing), &pull(State::Open, None));
        assert_eq!(unread.standing, read.standing);
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
                receipt_for: BTreeSet::new(),
                partial: false,
                lines: 0,
                after: None,
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
