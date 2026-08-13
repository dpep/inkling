//! Slack, reached by driving the Slack MCP server through a `claude`
//! subprocess.
//!
//! This exists because the Slack CLI isn't usable in the current environment.
//! It is a transport, not a decision-maker: the model is asked to call one
//! tool and hand back what it got, never to judge whether a PR is approved.
//!
//! The same code backs the `slackmcp` binary, so the bridge is usable by hand
//! and swappable for the real Slack API later without the domain noticing.

mod parse;
mod runner;

pub use parse::{Reaction, SearchHit, parse_reactions, parse_search};
pub use runner::{ClaudeRunner, Runner, RunnerError, extract_result};

use crate::domain::model::{Message, MessageId, ReactionName};
use crate::domain::window::SearchWindow;
use crate::ports::{MessageSource, ReactionSink, Scope, SinkError, SourceError};
use std::collections::HashMap;

/// Tool names are allowlisted per call, so a prompt-injection attempt in a
/// Slack message has nothing to reach for. The server also exposes
/// send_message and canvas writes; neither is ever permitted.
const SEARCH_TOOL: &str = "mcp__claude_ai_Slack__slack_search_public_and_private";
const READ_TOOL: &str = "mcp__claude_ai_Slack__slack_read_channel";
const REACT_TOOL: &str = "mcp__claude_ai_Slack__slack_add_reaction";

pub struct SlackClaude<R: Runner> {
    runner: R,
}

impl<R: Runner> SlackClaude<R> {
    pub fn new(runner: R) -> Self {
        Self { runner }
    }
}

/// Build the search query for a scope. Scope modifiers only — no keyword.
/// URL-shaped queries don't tokenize in Slack search, and local detection is
/// free, so the filter is never outsourced to the search index.
pub fn search_query(scope: &Scope, user_id: &str) -> String {
    match scope {
        Scope::Personal => format!("from:<@{user_id}>"),
        Scope::Channel(channel) => {
            let name = channel.trim_start_matches('#');
            format!("in:#{name}")
        }
    }
}

fn search_prompt(query: &str, window: &SearchWindow) -> String {
    // The model's whole job: one call, then hand back what came out. The
    // schema is spelled out because the tool returns rendered Markdown, and
    // a shape we can validate is worth more than one we have to scrape.
    format!(
        "Call {SEARCH_TOOL} with these exact arguments:\n\
         query: {query:?}\n\
         after: {:?}\n\
         before: {:?}\n\
         limit: 20\n\
         include_context: false\n\
         response_format: \"detailed\"\n\
         sort: \"timestamp\"\n\n\
         Then convert every result into this JSON, and output ONLY the JSON \
         with no commentary and no code fence:\n\
         {{\"messages\": [{{\"channel_id\": \"...\", \"ts\": \"...\", \
         \"user_id\": \"...\", \"permalink\": \"...\", \"text\": \"...\"}}]}}\n\n\
         Copy each field verbatim from the tool output. Do not summarize, \
         re-word, or omit any message. If there are no results, output \
         {{\"messages\": []}}.",
        window.start.timestamp().to_string(),
        window.end.timestamp().to_string(),
    )
}

fn reactions_prompt(channel_id: &str, oldest: &str) -> String {
    format!(
        "Call {READ_TOOL} with these exact arguments:\n\
         channel_id: {channel_id:?}\n\
         oldest: {oldest:?}\n\
         limit: 100\n\
         response_format: \"detailed\"\n\n\
         Then output ONLY this JSON, no commentary and no code fence:\n\
         {{\"messages\": [{{\"ts\": \"...\", \"reactions\": [\"name\", ...]}}]}}\n\n\
         List every message the tool returned. Give an empty reactions array \
         for a message with none. Use reaction names exactly as shown, \
         without colons or counts.",
    )
}

impl<R: Runner> MessageSource for SlackClaude<R> {
    fn discover(&self, scope: &Scope, window: &SearchWindow) -> Result<Vec<Message>, SourceError> {
        if window.is_empty() {
            return Ok(Vec::new());
        }
        // A user_id is only needed for personal scope; channel scope ignores it.
        let query = match scope {
            Scope::Personal => {
                return Err(SourceError::Search(
                    "personal scope needs a user id; use discover_personal".into(),
                ));
            }
            other => search_query(other, ""),
        };
        self.discover_with_query(&query, window)
    }

    fn reactions(
        &self,
        ids: &[MessageId],
    ) -> Result<HashMap<MessageId, Vec<ReactionName>>, SourceError> {
        let mut found = HashMap::new();
        // read_channel is per-channel, so group first: one call per channel
        // rather than one per message.
        let mut by_channel: HashMap<&str, Vec<&MessageId>> = HashMap::new();
        for id in ids {
            by_channel.entry(id.channel.as_str()).or_default().push(id);
        }

        for (channel, wanted) in by_channel {
            let earliest = wanted.iter().map(|id| id.ts.as_str()).min().unwrap_or("0");
            let oldest = exclusive_bound(earliest);
            let raw = self
                .runner
                .run(&reactions_prompt(channel, &oldest), &[READ_TOOL])
                .map_err(|e| SourceError::Transport(e.to_string()))?;
            let parsed = parse_reactions(&raw).map_err(SourceError::Search)?;

            for reaction in parsed {
                let id = MessageId::new(channel, &reaction.ts);
                if wanted.contains(&&id) {
                    found.insert(id, reaction.names);
                }
            }
        }
        Ok(found)
    }
}

impl<R: Runner> SlackClaude<R> {
    /// Personal scope needs the configured user id, which the port's
    /// signature doesn't carry.
    pub fn discover_personal(
        &self,
        user_id: &str,
        window: &SearchWindow,
    ) -> Result<Vec<Message>, SourceError> {
        self.discover_with_query(&search_query(&Scope::Personal, user_id), window)
    }

    fn discover_with_query(
        &self,
        query: &str,
        window: &SearchWindow,
    ) -> Result<Vec<Message>, SourceError> {
        let raw = self
            .runner
            .run(&search_prompt(query, window), &[SEARCH_TOOL])
            .map_err(|e| SourceError::Transport(e.to_string()))?;
        let hits = parse_search(&raw).map_err(SourceError::Search)?;
        Ok(hits.into_iter().map(SearchHit::into_message).collect())
    }
}

impl<R: Runner> ReactionSink for SlackClaude<R> {
    fn add(&self, target: &MessageId, emoji: &ReactionName) -> Result<(), SinkError> {
        let prompt = format!(
            "Call {REACT_TOOL} with these exact arguments:\n\
             channel_id: {:?}\n\
             message_ts: {:?}\n\
             emoji: {:?}\n\n\
             Then output ONLY the word OK, or the error code if the call failed.",
            target.channel,
            target.ts,
            emoji.as_str(),
        );
        let raw = self
            .runner
            .run(&prompt, &[REACT_TOOL])
            .map_err(|e| SinkError::Transport(e.to_string()))?;
        interpret_add(&raw, emoji)
    }
}

/// `slack_read_channel`'s `oldest` is exclusive — passing a message's own
/// timestamp reads everything *after* it and skips the message itself, which
/// silently reported "no reactions" for exactly the message being asked
/// about. Step back a second so the bound includes it.
fn exclusive_bound(ts: &str) -> String {
    let seconds = ts.split_once('.').map_or(ts, |(s, _)| s);
    match seconds.parse::<i64>() {
        Ok(n) => format!("{}.000000", n.saturating_sub(1)),
        Err(_) => "0".to_string(),
    }
}

/// Slack's error codes come through the bridge intact, so they can be matched
/// rather than guessed at from prose.
fn interpret_add(raw: &str, emoji: &ReactionName) -> Result<(), SinkError> {
    let text = raw.to_lowercase();
    if text.contains("invalid_name") {
        return Err(SinkError::InvalidEmoji(emoji.clone()));
    }
    if text.contains("message_not_found") || text.contains("channel_not_found") {
        return Err(SinkError::MessageNotFound);
    }
    // A duplicate add reports success exactly like a real one — which is why
    // ownership is decided from evidence beforehand, never from this result.
    if text.contains("ok") || text.contains("success") {
        return Ok(());
    }
    Err(SinkError::Transport(raw.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_use_scope_modifiers_only() {
        assert_eq!(
            search_query(&Scope::Personal, "U0E48AHQA"),
            "from:<@U0E48AHQA>"
        );
        assert_eq!(
            search_query(&Scope::Channel("#inkling".into()), ""),
            "in:#inkling"
        );
        // A channel configured without the hash still works.
        assert_eq!(
            search_query(&Scope::Channel("inkling".into()), ""),
            "in:#inkling"
        );
    }

    #[test]
    fn the_search_prompt_carries_unix_timestamps() {
        let window = SearchWindow {
            start: "2026-08-12T21:00:00Z".parse().unwrap(),
            end: "2026-08-12T21:30:00Z".parse().unwrap(),
        };
        let prompt = search_prompt("in:#inkling", &window);
        assert!(prompt.contains("1786568400"), "{prompt}");
        assert!(prompt.contains("1786570200"), "{prompt}");
    }

    #[test]
    fn the_read_bound_includes_the_message_it_names() {
        // Observed: passing a message's own ts as `oldest` skips it entirely,
        // which read as "no reactions" for the one message we asked about.
        assert_eq!(exclusive_bound("1786596050.245879"), "1786596049.000000");
        assert_eq!(exclusive_bound("nonsense"), "0");
    }

    #[test]
    fn a_successful_add_is_recognized() {
        let emoji = ReactionName::new("merged");
        assert!(interpret_add("OK", &emoji).is_ok());
        assert!(
            interpret_add(
                "Successfully added :merged: reaction to the message.",
                &emoji
            )
            .is_ok()
        );
    }

    #[test]
    fn slack_error_codes_map_to_typed_errors() {
        let emoji = ReactionName::new("nope");
        assert!(matches!(
            interpret_add("execution_failed: invalid_name", &emoji),
            Err(SinkError::InvalidEmoji(_))
        ));
        assert!(matches!(
            interpret_add("execution_failed: message_not_found", &emoji),
            Err(SinkError::MessageNotFound)
        ));
    }

    #[test]
    fn an_unrecognized_reply_is_a_transport_error_not_a_silent_success() {
        let emoji = ReactionName::new("merged");
        assert!(matches!(
            interpret_add("I'm not sure what you mean.", &emoji),
            Err(SinkError::Transport(_))
        ));
    }
}
