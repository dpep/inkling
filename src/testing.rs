//! Fakes standing in for Slack, GitHub, and the clock.
//!
//! These make the whole pipeline assertable without a network: a scripted
//! provider can walk a PR through its lifecycle across runs, and a recording
//! sink captures exactly what would have been written.

use crate::domain::model::{Message, MessageId, ReactionName, Reference, State};
use crate::domain::wait::CheckState;
use crate::domain::window::SearchWindow;
use crate::ports::*;
use chrono::{DateTime, Utc};
use std::cell::RefCell;
use std::collections::HashMap;

pub struct FixedClock(pub RefCell<DateTime<Utc>>);

impl FixedClock {
    pub fn at(t: &str) -> Self {
        Self(RefCell::new(t.parse().expect("valid timestamp")))
    }

    pub fn advance(&self, by: chrono::TimeDelta) {
        *self.0.borrow_mut() += by;
    }
}

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.borrow()
    }
}

#[derive(Default)]
pub struct FakeMessageSource {
    pub messages: Vec<Message>,
    pub reactions: RefCell<HashMap<MessageId, Vec<ReactionName>>>,
    /// When true, `reactions` reports failure — the "evidence unknown" path.
    pub reactions_unavailable: bool,
}

impl FakeMessageSource {
    pub fn with(messages: Vec<Message>) -> Self {
        Self {
            messages,
            ..Default::default()
        }
    }

    pub fn set_reactions(&self, id: &MessageId, names: &[&str]) {
        self.reactions
            .borrow_mut()
            .insert(id.clone(), names.iter().map(ReactionName::new).collect());
    }
}

impl MessageSource for FakeMessageSource {
    fn discover(&self, _scope: &Scope, window: &SearchWindow) -> Result<Discovered, SourceError> {
        let mut messages: Vec<Message> = self
            .messages
            .iter()
            .filter(|m| m.posted_at >= window.start && m.posted_at <= window.end)
            .cloned()
            .collect();
        // Oldest first, like the real source.
        messages.sort_by_key(|m| m.posted_at);
        Ok(Discovered::all(messages))
    }

    fn reactions(
        &self,
        ids: &[MessageId],
    ) -> Result<HashMap<MessageId, Vec<ReactionName>>, SourceError> {
        if self.reactions_unavailable {
            return Err(SourceError::Transport("reactions unavailable".into()));
        }
        let all = self.reactions.borrow();
        Ok(ids
            .iter()
            .map(|id| (id.clone(), all.get(id).cloned().unwrap_or_default()))
            .collect())
    }
}

/// A provider whose answers can be rewritten between runs, so a test can walk
/// a PR from open to approved to merged and assert what each run does.
#[derive(Default)]
pub struct ScriptedStatusProvider {
    states: RefCell<HashMap<Reference, Result<RefStatus, RefError>>>,
    lookups: RefCell<usize>,
}

impl ScriptedStatusProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, reference: &Reference, state: State) {
        self.states
            .borrow_mut()
            .insert(reference.clone(), Ok(RefStatus::new(state)));
    }

    /// For display assertions, where who opened the PR is the point.
    pub fn set_with_author(&self, reference: &Reference, state: State, author: &str) {
        self.states.borrow_mut().insert(
            reference.clone(),
            Ok(RefStatus {
                author: author.to_string(),
                ..RefStatus::new(state)
            }),
        );
    }

    /// For waits on CI, where the lifecycle state is beside the point.
    pub fn set_checks(&self, reference: &Reference, state: State, checks: CheckState) {
        self.states.borrow_mut().insert(
            reference.clone(),
            Ok(RefStatus {
                checks: Some(checks),
                ..RefStatus::new(state)
            }),
        );
    }

    /// For watches on CI, where naming the red job is the point.
    pub fn fail_check(&self, reference: &Reference, state: State, failing: &str) {
        self.states.borrow_mut().insert(
            reference.clone(),
            Ok(RefStatus {
                checks: Some(CheckState::Failure),
                failing: Some(failing.to_string()),
                ..RefStatus::new(state)
            }),
        );
    }

    /// How many times the provider was actually asked — the measurement
    /// deduplicated polling exists to keep down.
    pub fn calls(&self) -> usize {
        *self.lookups.borrow()
    }

    pub fn fail(&self, reference: &Reference, error: RefError) {
        self.states
            .borrow_mut()
            .insert(reference.clone(), Err(error));
    }
}

impl StatusProvider for ScriptedStatusProvider {
    fn lookup(&self, references: &[Reference]) -> Result<Vec<Lookup>, ProviderError> {
        *self.lookups.borrow_mut() += 1;
        let states = self.states.borrow();
        Ok(references
            .iter()
            .map(|r| {
                let state = states.get(r).cloned().unwrap_or(Err(RefError::NotFound));
                (r.clone(), state)
            })
            .collect())
    }
}

/// Captures every write, so tests assert the exact sequence rather than a
/// final state — which is what catches a redundant or out-of-order add.
#[derive(Default)]
pub struct RecordingSink {
    pub added: RefCell<Vec<(MessageId, ReactionName)>>,
    pub fail_with: Option<fn(&ReactionName) -> Option<SinkError>>,
}

impl RecordingSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn emoji_names(&self) -> Vec<String> {
        self.added
            .borrow()
            .iter()
            .map(|(_, e)| e.to_string())
            .collect()
    }
}

impl ReactionSink for RecordingSink {
    fn add(&self, target: &MessageId, emoji: &ReactionName) -> Result<(), SinkError> {
        if let Some(fail) = self.fail_with
            && let Some(error) = fail(emoji)
        {
            return Err(error);
        }
        self.added
            .borrow_mut()
            .push((target.clone(), emoji.clone()));
        Ok(())
    }
}

/// Build a message for tests. Defaults to the configured user and no known
/// reactions.
pub fn message(id: &str, text: &str, posted_at: &str) -> Message {
    Message {
        id: MessageId::new("C_TEST", id),
        channel_name: "test".to_string(),
        text: text.to_string(),
        author: "U_ME".to_string(),
        author_name: "me".to_string(),
        posted_at: posted_at.parse().expect("valid timestamp"),
        permalink: format!("https://example.slack.com/archives/C_TEST/p{id}"),
        reactions: None,
    }
}
