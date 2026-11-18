//! The seams between the domain and the outside world.
//!
//! Everything here is a trait the core calls and an adapter implements. The
//! core never learns whether Slack is reached over MCP or the Web API, or
//! whether state lives in a file or a database.

use crate::domain::model::{Message, MessageId, ReactionName, Reference, State};
use crate::domain::wait::CheckState;
use crate::domain::watch::{Actors, Category, Discussion, FailedCheck, Head};
use crate::domain::window::SearchWindow;
use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, HashMap};

/// Where to look for messages.
///
/// Personal scope carries the user ID rather than leaving the adapter to find
/// it: a source that can't answer "who am I" would otherwise have to refuse
/// the scope outright, which is exactly what used to happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Messages this user wrote, wherever they wrote them.
    Personal(String),
    Channel(String),
}

impl Scope {
    /// Stable key for remembering how far this scope has been searched.
    ///
    /// Channels normalize, so `#eng` and `eng` are one scope rather than two
    /// that each re-scan the other's history.
    pub fn key(&self) -> String {
        match self {
            Scope::Personal(_) => "personal".to_string(),
            Scope::Channel(c) => format!("channel:{}", c.trim_start_matches('#').to_lowercase()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("slack search failed: {0}")]
    Search(String),
    #[error("slack transport failed: {0}")]
    Transport(String),
}

/// One search's worth of messages, and whether it exhausted the window.
///
/// Slack caps a search page, so a wide window over a busy channel returns a
/// prefix. Saying so lets the caller advance its mark only as far as it
/// actually covered, and pick the rest up next run — rather than skipping it.
#[derive(Debug, Default)]
pub struct Discovered {
    pub messages: Vec<Message>,
    pub complete: bool,
}

impl Discovered {
    /// Everything the window held.
    pub fn all(messages: Vec<Message>) -> Self {
        Self {
            messages,
            complete: true,
        }
    }
}

pub trait MessageSource {
    /// Messages in scope within the window, oldest first. May over-return;
    /// the caller filters precisely.
    fn discover(&self, scope: &Scope, window: &SearchWindow) -> Result<Discovered, SourceError>;

    /// Current reactions for tracked messages.
    ///
    /// Search results don't carry reactions, so this is a separate call. It
    /// is what supplies reconciliation's evidence — and therefore the only
    /// thing that can establish whether an emoji was ours.
    fn reactions(
        &self,
        ids: &[MessageId],
    ) -> Result<HashMap<MessageId, Vec<ReactionName>>, SourceError>;

    /// Human names for user IDs. Defaulted empty: a source that can't answer
    /// should degrade to showing whatever the message already carried, not
    /// fail the run over a display detail.
    fn user_names(&self, _ids: &[String]) -> Result<HashMap<String, String>, SourceError> {
        Ok(HashMap::new())
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ProviderError {
    #[error("github request failed: {0}")]
    Transport(String),
    #[error("slack read failed: {0}")]
    Slack(String),
}

/// Why one reference couldn't be resolved, while its siblings could.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RefError {
    #[error("not found or not visible")]
    NotFound,
    #[error("{0}")]
    Other(String),
}

/// What a provider knows about one reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefStatus {
    pub state: State,
    /// Who opened it. Display only — worth showing because the person who
    /// posted the link often isn't the person who wrote the code.
    pub author: String,
    /// Display only. A bare `owner/repo#12` says nothing about what it is.
    pub title: String,
    /// Where CI stands, or `None` when the repository runs none.
    ///
    /// Read by `wait` and by nothing else. It deliberately never reaches the
    /// tracked snapshot: a finished suite can go back to pending on a push,
    /// and the snapshot's terminality means "stop polling forever".
    pub checks: Option<CheckState>,
    /// A check that is red, when one is. "Checks went red" is most of a page
    /// of investigation; "checks went red: tidy" is a command.
    pub failing: Option<FailedCheck>,
    /// The branch its commits come from, so a push to it can be recognised.
    pub head: Option<Head>,
    /// Who did what to it, so a transition can say.
    pub by: Actors,
    /// Who has said something on it lately, and when. Never what.
    pub discussion: Option<Discussion>,
}

impl RefStatus {
    pub fn new(state: State) -> Self {
        Self {
            state,
            author: String::new(),
            title: String::new(),
            checks: None,
            failing: None,
            head: None,
            by: Default::default(),
            discussion: None,
        }
    }
}

/// One reference's outcome. The inner `Result` is the in-band part: a
/// reference nobody can read shouldn't take its siblings down with it.
pub type Lookup = (Reference, Result<RefStatus, RefError>);

pub trait StatusProvider {
    /// Batched lookup.
    fn lookup(&self, references: &[Reference]) -> Result<Vec<Lookup>, ProviderError>;
}

/// What an issue tracker knows about one issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueStatus {
    /// The status as the project spells it — "In Review", not a category.
    /// This is what an event is about, so it isn't normalized away.
    pub name: String,
    pub category: Category,
    /// Display only, so an event reads without a second lookup.
    pub summary: String,
}

/// Issues, kept separate from `StatusProvider` rather than generalized with
/// it. The two upstreams answer different questions and their shapes have
/// nothing in common but the word "status" — one ordered ladder plus a CI
/// rollup, one project-defined name plus a category. A common interface here
/// would be a union with a discriminant, which is what `Standing` already is,
/// one layer up where it earns itself. A third tracker is when this becomes a
/// registry.
pub trait IssueProvider {
    fn read(&self, keys: &[String]) -> Result<Vec<IssueLookup>, ProviderError>;
}

/// One issue's outcome, in-band like `Lookup`.
pub type IssueLookup = (String, Result<IssueStatus, RefError>);

/// What a Slack message has gathered since it was posted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageStatus {
    /// Emoji → how many. The bridge reads counts, not who reacted.
    pub reactions: BTreeMap<String, u32>,
    /// The replies after it in its thread: ts → the user id that wrote it.
    pub replies: BTreeMap<String, String>,
    /// User id → name, for everyone the read met, so a push names people.
    pub names: BTreeMap<String, String>,
}

/// One message's outcome, in-band like `Lookup`.
pub type MessageLookup = (Reference, Result<MessageStatus, RefError>);

/// Slack messages, a third provider beside pull requests and issues for the
/// same reason those two are separate: nothing in its answer is shaped like
/// theirs.
pub trait MessageProvider {
    fn read(&self, messages: &[Reference]) -> Result<Vec<MessageLookup>, ProviderError>;

    /// The Slack user the credentials belong to, so what the reader did
    /// themself can be told apart. `None` when it can't be found out.
    fn me(&self) -> Option<String>;
}

/// Where a wait gets its readings.
///
/// Identical in shape to `StatusProvider` and deliberately separate: a wait
/// asks "what is true now", on behalf of someone blocked, while the run loop
/// asks on a cron cadence. Naming the seam is what lets one waiter poll on
/// everyone's behalf without the callers knowing (docs/waiting.md, W3), and
/// is where webhook delivery would land if polling ever stops being the
/// mechanism.
pub trait Observer {
    fn observe(&self, references: &[Reference]) -> Result<Vec<Lookup>, ProviderError>;
}

/// Trivially true for the direct case: a lone waiter just asks GitHub.
impl<T: StatusProvider> Observer for T {
    fn observe(&self, references: &[Reference]) -> Result<Vec<Lookup>, ProviderError> {
        self.lookup(references)
    }
}

/// Passing time. Behind a trait so a test of a thirty-minute wait takes no
/// longer than any other test.
pub trait Sleeper {
    fn sleep(&self, duration: std::time::Duration);
}

pub struct RealSleeper;

impl Sleeper for RealSleeper {
    fn sleep(&self, duration: std::time::Duration) {
        std::thread::sleep(duration);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SinkError {
    /// The workspace has no such emoji. Retrying won't help.
    #[error("no such emoji in this workspace: {0}")]
    InvalidEmoji(ReactionName),
    /// The message is gone; drop the record.
    #[error("message not found")]
    MessageNotFound,
    #[error("slack transport failed: {0}")]
    Transport(String),
}

pub trait ReactionSink {
    /// Add a reaction. Adding one that's already there is a success — Slack
    /// reports no difference, which is why the ledger decision is made from
    /// evidence beforehand rather than from this result.
    fn add(&self, target: &MessageId, emoji: &ReactionName) -> Result<(), SinkError>;
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("reading state: {0}")]
    Read(String),
    #[error("writing state: {0}")]
    Write(String),
    #[error("state was written by a newer version ({found}); refusing to overwrite it")]
    FutureVersion { found: u32 },
    /// Somebody else held the lock for longer than the caller could wait. Its
    /// own class because it is the one store failure that is nobody's fault
    /// and fixes itself: the next caller gets in.
    #[error("the store was busy for {waited_ms}ms")]
    Busy { waited_ms: u64 },
}

pub trait Clock {
    fn now(&self) -> DateTime<Utc>;
}

/// The real clock. Tests use a fixed one so time-dependent behavior is
/// asserted rather than waited for.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}
