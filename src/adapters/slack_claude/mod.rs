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

pub use parse::{Reaction, SearchHit, parse_channel_id, parse_reactions, parse_search};
pub use runner::{ClaudeRunner, Runner, RunnerError, extract_result};

use crate::domain::model::{Message, MessageId, ReactionName};
use crate::domain::window::SearchWindow;
use crate::ports::{MessageSource, ReactionSink, Scope, SinkError, SourceError};
use std::cell::RefCell;
use std::collections::HashMap;

/// Tool names are allowlisted per call, so a prompt-injection attempt in a
/// Slack message has nothing to reach for. The server also exposes
/// send_message and canvas writes; neither is ever permitted.
const SEARCH_TOOL: &str = "mcp__claude_ai_Slack__slack_search_public_and_private";
const READ_TOOL: &str = "mcp__claude_ai_Slack__slack_read_channel";
const REACT_TOOL: &str = "mcp__claude_ai_Slack__slack_add_reaction";
const CHANNELS_TOOL: &str = "mcp__claude_ai_Slack__slack_search_channels";
const THREAD_TOOL: &str = "mcp__claude_ai_Slack__slack_read_thread";
/// Only ever reachable from `slackmcp post` — deliberately not from anything
/// Inkling's run loop can call. Inkling annotates; it does not talk.
const SEND_TOOL: &str = "mcp__claude_ai_Slack__slack_send_message";

/// Slack channel IDs look like `C0BPR3AEAG3` — a `C`, `D`, or `G` followed by
/// uppercase alphanumerics. Anything else is a human-typed handle.
///
/// Distinguishing them locally matters: resolving a handle costs a bridge
/// invocation, and an ID that got resolved anyway would pay for nothing.
pub fn looks_like_channel_id(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('C' | 'D' | 'G'))
        && s.len() >= 9
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

pub struct SlackClaude<R: Runner> {
    runner: R,
    channel_ids: RefCell<HashMap<String, String>>,
}

/// A model occasionally answers with prose instead of doing the job — most
/// memorably, claiming it has no Slack tools when it plainly does. Observed
/// in practice, and a fresh call usually succeeds, so every operation gets a
/// bounded retry. Without this, a healthy run reports a spurious failure.
const ATTEMPTS: usize = 3;

impl<R: Runner> SlackClaude<R> {
    pub fn new(runner: R) -> Self {
        Self {
            runner,
            channel_ids: RefCell::new(HashMap::new()),
        }
    }

    /// Run a prompt and interpret the reply, retrying when the reply doesn't
    /// make sense. Transport errors are retried too; a typed refusal from
    /// Slack — a missing emoji, a deleted message — is not, since repeating
    /// it would just cost time.
    fn attempt<T>(
        &self,
        prompt: &str,
        tools: &[&str],
        interpret: impl Fn(&str) -> Result<T, Retry>,
    ) -> Result<T, String> {
        let mut last = String::from("no attempt made");
        for attempt in 0..ATTEMPTS {
            match self.runner.run(prompt, tools) {
                Ok(raw) => match interpret(&raw) {
                    Ok(value) => return Ok(value),
                    Err(Retry::Fatal(error)) => return Err(error),
                    Err(Retry::Again(error)) => last = error,
                },
                Err(error) => last = error.to_string(),
            }
            if attempt + 1 < ATTEMPTS {
                tracing::warn!(attempt = attempt + 1, error = %last, "bridge call failed, retrying");
            }
        }
        Err(format!("after {ATTEMPTS} attempts: {last}"))
    }
}

/// Whether a bad reply is worth another try.
enum Retry {
    Again(String),
    Fatal(String),
}

/// Build the search query for a scope. Scope modifiers only — no keyword.
/// URL-shaped queries don't tokenize in Slack search, and local detection is
/// free, so the filter is never outsourced to the search index.
pub fn search_query(scope: &Scope, user_id: &str) -> String {
    match scope {
        Scope::Personal => format!("from:<@{user_id}>"),
        Scope::Channel(channel) => {
            let name = channel.trim_start_matches('#');
            // Slack spells the two forms differently: `in:#name` for a handle,
            // `in:<#C123>` for an ID. Using the wrong one silently returns
            // nothing rather than erroring.
            if looks_like_channel_id(name) {
                format!("in:<#{name}>")
            } else {
                format!("in:#{name}")
            }
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
            for (id, names) in self.channel_reactions(channel, earliest)? {
                if wanted.contains(&&id) {
                    found.insert(id, names);
                }
            }
        }
        Ok(found)
    }
}

impl<R: Runner> SlackClaude<R> {
    /// Turn a channel handle into an ID, passing IDs straight through.
    ///
    /// Resolutions are memoized for the process: a run touching the same
    /// channel repeatedly shouldn't pay for the lookup more than once.
    pub fn resolve_channel(&self, channel: &str) -> Result<String, SourceError> {
        let name = channel.trim_start_matches('#');
        if looks_like_channel_id(name) {
            return Ok(name.to_string());
        }
        if let Some(known) = self.channel_ids.borrow().get(name) {
            return Ok(known.clone());
        }

        let prompt = format!(
            "Call {CHANNELS_TOOL} with query {name:?}.\n\n\
             Then output ONLY this JSON, no commentary and no code fence:\n\
             {{\"id\": \"...\"}}\n\n\
             Use the id of the channel named exactly {name:?}. If there is no \
             exact match, output {{\"id\": null}}.",
        );
        let id = self
            .attempt(&prompt, &[CHANNELS_TOOL], |raw| {
                match parse_channel_id(raw) {
                    // A clean "no such channel" is an answer, not a hiccup.
                    Ok(None) => Err(Retry::Fatal(format!("no channel named #{name}"))),
                    Ok(Some(id)) => Ok(id),
                    Err(error) => Err(Retry::Again(error)),
                }
            })
            .map_err(SourceError::Search)?;
        self.channel_ids
            .borrow_mut()
            .insert(name.to_string(), id.clone());
        Ok(id)
    }

    /// Personal scope needs the configured user id, which the port's
    /// signature doesn't carry.
    pub fn discover_personal(
        &self,
        user_id: &str,
        window: &SearchWindow,
    ) -> Result<Vec<Message>, SourceError> {
        self.discover_with_query(&search_query(&Scope::Personal, user_id), window)
    }

    /// Every message in a channel since `since`, with its reactions.
    ///
    /// The port's `reactions` narrows this to messages it already tracks;
    /// callers who want the whole channel — `slackmcp reactions` — use this.
    pub fn channel_reactions(
        &self,
        channel_id: &str,
        since: &str,
    ) -> Result<Vec<(MessageId, Vec<ReactionName>)>, SourceError> {
        let oldest = exclusive_bound(since);
        let parsed = self
            .attempt(
                &reactions_prompt(channel_id, &oldest),
                &[READ_TOOL],
                |raw| parse_reactions(raw).map_err(Retry::Again),
            )
            .map_err(SourceError::Search)?;

        Ok(parsed
            .into_iter()
            .map(|r| (MessageId::new(channel_id, &r.ts), r.names))
            .collect())
    }

    /// A thread's parent message and every reply, with reactions.
    pub fn thread(&self, channel_id: &str, parent_ts: &str) -> Result<Vec<Message>, SourceError> {
        let prompt = format!(
            "Call {THREAD_TOOL} with these exact arguments:\n\
             channel_id: {channel_id:?}\n\
             message_ts: {parent_ts:?}\n\
             limit: 100\n\
             response_format: \"detailed\"\n\n\
             Then output ONLY this JSON, no commentary and no code fence:\n\
             {{\"messages\": [{{\"channel_id\": \"...\", \"ts\": \"...\", \
             \"user_id\": \"...\", \"permalink\": \"\", \"text\": \"...\"}}]}}\n\n\
             Include the parent message first, then every reply in order. \
             Copy each field verbatim.",
        );
        let hits = self
            .attempt(&prompt, &[THREAD_TOOL], |raw| {
                parse_search(raw).map_err(Retry::Again)
            })
            .map_err(SourceError::Search)?;
        Ok(hits.into_iter().map(SearchHit::into_message).collect())
    }

    /// Post a message, optionally as a thread reply.
    ///
    /// Separate from everything the run loop touches, and gated behind its own
    /// tool allowlist — reaching this from a reconciliation path would take a
    /// deliberate code change, not a clever Slack message.
    pub fn post(
        &self,
        channel_id: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<(), SourceError> {
        let threading = thread_ts.map_or_else(String::new, |ts| format!("thread_ts: {ts:?}\n"));
        let prompt = format!(
            "Call {SEND_TOOL} with these exact arguments:\n\
             channel_id: {channel_id:?}\n\
             {threading}\
             message: {text:?}\n\n\
             Then output ONLY the word OK, or the error code if it failed.",
        );
        self.attempt(&prompt, &[SEND_TOOL], |raw| {
            let lower = raw.to_lowercase();
            if lower.contains("ok") || lower.contains("success") {
                Ok(())
            } else if lower.contains("not_found") {
                Err(Retry::Fatal(raw.trim().to_string()))
            } else {
                Err(Retry::Again(raw.trim().to_string()))
            }
        })
        .map_err(SourceError::Transport)
    }

    /// Run a raw Slack search query, for callers that already speak the
    /// modifier syntax.
    pub fn discover_raw(
        &self,
        query: &str,
        window: &SearchWindow,
    ) -> Result<Vec<Message>, SourceError> {
        self.discover_with_query(query, window)
    }

    fn discover_with_query(
        &self,
        query: &str,
        window: &SearchWindow,
    ) -> Result<Vec<Message>, SourceError> {
        let hits = self
            .attempt(&search_prompt(query, window), &[SEARCH_TOOL], |raw| {
                parse_search(raw).map_err(Retry::Again)
            })
            .map_err(SourceError::Search)?;
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
        self.attempt(&prompt, &[REACT_TOOL], |raw| {
            match interpret_add(raw, emoji) {
                Ok(()) => Ok(()),
                // Slack said no for a reason that won't change on a retry.
                Err(e @ (SinkError::InvalidEmoji(_) | SinkError::MessageNotFound)) => {
                    Err(Retry::Fatal(e.to_string()))
                }
                Err(e) => Err(Retry::Again(e.to_string())),
            }
        })
        .map_err(|error| classify_sink_error(&error, emoji))
    }
}

/// Rebuild a typed error after the retry loop has flattened it to a string,
/// so callers can still tell a missing emoji from a transport problem.
fn classify_sink_error(error: &str, emoji: &ReactionName) -> SinkError {
    if error.contains("no such emoji") {
        SinkError::InvalidEmoji(emoji.clone())
    } else if error.contains("message not found") {
        SinkError::MessageNotFound
    } else {
        SinkError::Transport(error.to_string())
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

    /// Replays canned replies in order, so a flaky bridge is reproducible.
    struct ScriptedRunner {
        replies: RefCell<Vec<String>>,
        calls: RefCell<usize>,
    }

    impl ScriptedRunner {
        fn new(replies: &[&str]) -> Self {
            Self {
                replies: RefCell::new(replies.iter().rev().map(|s| (*s).to_string()).collect()),
                calls: RefCell::new(0),
            }
        }
    }

    impl Runner for ScriptedRunner {
        fn run(&self, _prompt: &str, _tools: &[&str]) -> Result<String, RunnerError> {
            *self.calls.borrow_mut() += 1;
            Ok(self.replies.borrow_mut().pop().unwrap_or_default())
        }
    }

    #[test]
    fn a_bridge_hiccup_is_retried() {
        // Observed in a live run: the model claimed it had no Slack tools,
        // then worked fine on the next call.
        let runner = ScriptedRunner::new(&["I don't have access to any Slack MCP tools.", "OK"]);
        let slack = SlackClaude::new(runner);
        let result = slack.add(&MessageId::new("C1", "1.0"), &ReactionName::new("merged"));
        assert!(result.is_ok());
    }

    #[test]
    fn a_refusal_slack_actually_made_is_not_retried() {
        // Retrying a missing emoji just burns time and money.
        let runner = ScriptedRunner::new(&["execution_failed: invalid_name", "OK"]);
        let slack = SlackClaude::new(runner);
        let error = slack
            .add(&MessageId::new("C1", "1.0"), &ReactionName::new("nope"))
            .unwrap_err();
        assert!(matches!(error, SinkError::InvalidEmoji(_)), "{error:?}");
    }

    #[test]
    fn giving_up_reports_the_last_failure() {
        let runner = ScriptedRunner::new(&["nonsense", "more nonsense", "still nonsense"]);
        let slack = SlackClaude::new(runner);
        let error = slack
            .add(&MessageId::new("C1", "1.0"), &ReactionName::new("merged"))
            .unwrap_err();
        assert!(error.to_string().contains("3 attempts"), "{error}");
    }

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
    fn a_channel_id_uses_slacks_other_spelling() {
        // `in:#C0BPR3AEAG3` is accepted and matches nothing, so getting this
        // wrong looks like an empty channel rather than an error.
        assert_eq!(
            search_query(&Scope::Channel("C0BPR3AEAG3".into()), ""),
            "in:<#C0BPR3AEAG3>"
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
    fn channel_ids_are_told_apart_from_handles_without_a_lookup() {
        // Resolving a handle costs a bridge call, so an ID must never take
        // that path.
        assert!(looks_like_channel_id("C0BPR3AEAG3"));
        assert!(looks_like_channel_id("D0AS6PZ20AF"));
        assert!(!looks_like_channel_id("inkling"));
        assert!(!looks_like_channel_id("eng-prs"));
        assert!(!looks_like_channel_id("Coding"), "mixed case is a handle");
        assert!(!looks_like_channel_id("C123"), "too short to be an id");
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
