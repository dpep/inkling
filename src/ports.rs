//! The seams between the domain and the outside world.
//!
//! Everything here is a trait the core calls and an adapter implements. The
//! core never learns whether Slack is reached over MCP or the Web API, or
//! whether state lives in a file or a database.

use crate::domain::model::{Message, MessageId, ReactionName, Reference, State};
use crate::domain::window::SearchWindow;
use chrono::{DateTime, Utc};
use std::collections::HashMap;

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

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("slack search failed: {0}")]
    Search(String),
    #[error("slack transport failed: {0}")]
    Transport(String),
}

pub trait MessageSource {
    /// Messages in scope within the window. May over-return; the caller
    /// filters precisely.
    fn discover(&self, scope: &Scope, window: &SearchWindow) -> Result<Vec<Message>, SourceError>;

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

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("github request failed: {0}")]
    Transport(String),
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
}

impl RefStatus {
    pub fn new(state: State) -> Self {
        Self {
            state,
            author: String::new(),
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
