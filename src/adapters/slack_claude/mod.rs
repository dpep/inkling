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

pub use parse::{
    Activity, ChannelRead, Identity, Reaction, SearchHit, ThreadEntry, parse_activity,
    parse_channel_id, parse_channel_read, parse_identity, parse_reactions, parse_search,
};
#[cfg(test)]
pub use parse::{
    REAL_CHANNEL_BASELINE, REAL_CHANNEL_READ, REAL_DM_ACTIVITY, REAL_EMPTY_CHANNEL, REAL_THREAD,
};
pub use runner::{BRIDGE_ENV, ClaudeRunner, Runner, RunnerError, extract_result};

use crate::config::SlackMcp;
use crate::domain::model::{Message, MessageId, ReactionName};
use crate::domain::window::SearchWindow;
use crate::ports::{Discovered, MessageSource, ReactionSink, Scope, SinkError, SourceError};
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

/// The account-linked connector this has always used. Still the default, so
/// an absent `[slack.mcp]` changes nothing.
const DEFAULT_SERVER: &str = "claude_ai_Slack";

/// Fully-qualified tool names, by role.
///
/// Names are allowlisted per call, so a prompt-injection attempt in a Slack
/// message has nothing to reach for. `send` is deliberately unreachable from
/// anything the run loop calls — Inkling annotates; it does not talk.
#[derive(Debug, Clone)]
pub struct ToolNames {
    pub search: String,
    pub read_channel: String,
    pub react: String,
    pub search_channels: String,
    pub read_thread: String,
    pub read_user_profile: String,
    pub send: String,
}

impl ToolNames {
    pub fn new(mcp: Option<&SlackMcp>) -> Self {
        let server = mcp.map_or(DEFAULT_SERVER, |m| m.server.as_str());
        let tools = mcp.map(|m| m.tools.clone()).unwrap_or_default();
        let qualify = |tool: &str| format!("mcp__{server}__{tool}");
        Self {
            search: qualify(&tools.search),
            read_channel: qualify(&tools.read_channel),
            react: qualify(&tools.react),
            search_channels: qualify(&tools.search_channels),
            read_thread: qualify(&tools.read_thread),
            read_user_profile: qualify(&tools.read_user_profile),
            send: qualify(&tools.send_message),
        }
    }
}

impl Default for ToolNames {
    fn default() -> Self {
        Self::new(None)
    }
}

/// Build the bridge from config: the same wiring both binaries need, so
/// neither can configure the transport differently by accident.
pub fn from_config(
    config: &crate::config::Config,
    model: Option<String>,
) -> Result<SlackClaude<ClaudeRunner>, crate::config::ConfigError> {
    let mcp = config.slack.mcp.as_ref();
    let runner = ClaudeRunner {
        mcp_config: mcp.map(|m| m.config_json()).transpose()?,
        model: model.or_else(|| ClaudeRunner::default().model),
        ..ClaudeRunner::default()
    };
    Ok(SlackClaude::with_tools(runner, ToolNames::new(mcp)))
}

/// Slack channel IDs look like `C012XYZ6789` — a `C`, `D`, or `G` followed by
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
    tools: ToolNames,
    /// Injectable so tests exercising the retry path don't sleep through it.
    retry_delay: Duration,
    channel_ids: RefCell<HashMap<String, String>>,
    user_names: RefCell<HashMap<String, String>>,
}

/// A model occasionally answers with prose instead of doing the job — most
/// memorably, claiming it has no Slack tools when it plainly does. Observed
/// in practice, and a fresh call usually succeeds, so every operation gets a
/// bounded retry. Without this, a healthy run reports a spurious failure.
const ATTEMPTS: usize = 3;

/// Wait before retrying. Not exponential — this is three tries against a
/// transient hiccup, not a rate limit — but back-to-back retries give an
/// upstream blip no time at all to clear.
const RETRY_DELAY: Duration = Duration::from_millis(1500);

/// The tool's own maximum. A full page means there is more behind it.
pub const PAGE_LIMIT: usize = 20;

/// `slack_read_channel`'s maximum page.
pub const CHANNEL_PAGE: usize = 100;

/// `slack_read_thread`'s maximum page. It lists oldest first, so a thread
/// longer than a page loses its newest replies, the ones that are news.
const THREAD_PAGE: usize = 1000;

impl<R: Runner> SlackClaude<R> {
    pub fn new(runner: R) -> Self {
        Self::with_tools(runner, ToolNames::default())
    }

    pub fn with_tools(runner: R, tools: ToolNames) -> Self {
        Self {
            runner,
            tools,
            retry_delay: RETRY_DELAY,
            channel_ids: RefCell::new(HashMap::new()),
            user_names: RefCell::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    pub(crate) fn without_delay(mut self) -> Self {
        self.retry_delay = Duration::ZERO;
        self
    }

    /// Run a prompt and interpret the reply, retrying when the reply doesn't
    /// make sense. Transport errors are retried too, except a timeout: a hang
    /// costs the whole timeout again. A typed refusal from Slack — a missing
    /// emoji, a deleted message — is not, since repeating it would just cost
    /// time.
    fn attempt<T>(
        &self,
        prompt: &str,
        tools: &[&str],
        interpret: impl Fn(&str) -> Result<T, Retry>,
    ) -> Result<T, String> {
        let mut last = String::from("no attempt made");
        for attempt in 0..ATTEMPTS {
            if attempt > 0 && !self.retry_delay.is_zero() {
                std::thread::sleep(self.retry_delay);
            }
            match self.runner.run(prompt, tools, attempt) {
                Ok(raw) => match interpret(&raw) {
                    Ok(value) => {
                        // Worth one line when it took more than one go, and
                        // silence when it didn't. A recovered call logged at
                        // WARN reads like breakage on a run that succeeded.
                        if attempt > 0 {
                            tracing::info!(attempts = attempt + 1, "bridge call recovered");
                        }
                        return Ok(value);
                    }
                    Err(Retry::Fatal(error)) => return Err(error),
                    Err(Retry::Again(error)) => last = error,
                },
                Err(error @ RunnerError::TimedOut { .. }) => return Err(error.to_string()),
                Err(error) => last = error.to_string(),
            }
            if attempt + 1 < ATTEMPTS {
                tracing::debug!(attempt = attempt + 1, error = %last, "bridge call failed, retrying");
            }
        }
        Err(format!("after {ATTEMPTS} attempts: {last}"))
    }
}

/// Slack's codes for a thread that isn't there to read. They arrive through
/// the bridge intact (docs/slack-mcp-spike.md), so they are matched, not
/// guessed at.
const GONE: [&str; 3] = ["thread_not_found", "message_not_found", "channel_not_found"];
const GONE_MARK: &str = "gone";

/// A reply that didn't parse: Slack saying the thing is gone, or a model to
/// ask again. Only then — a good read can carry a code in a display name.
fn unparsed(raw: &str, error: String) -> Retry {
    match GONE.iter().any(|code| raw.contains(code)) {
        true => Retry::Fatal(format!("{GONE_MARK}: {}", raw.trim())),
        false => Retry::Again(error),
    }
}

/// Why a thread couldn't be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityError {
    /// Slack says it isn't there, or not to these credentials.
    Gone(String),
    Failed(String),
}

/// Whether a bad reply is worth another try.
enum Retry {
    Again(String),
    Fatal(String),
}

/// Build the search query for a scope. Scope modifiers only — no keyword.
/// URL-shaped queries don't tokenize in Slack search, and local detection is
/// free, so the filter is never outsourced to the search index.
pub fn search_query(scope: &Scope) -> String {
    match scope {
        Scope::Personal(user_id) => format!("from:<@{user_id}>"),
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

fn search_prompt(tool: &str, query: &str, window: &SearchWindow) -> String {
    // The model's whole job: one call, then hand back what came out. The
    // schema is spelled out because the tool returns rendered Markdown, and
    // a shape we can validate is worth more than one we have to scrape.
    format!(
        "Call {tool} with these exact arguments:\n\
         query: {query:?}\n\
         after: {:?}\n\
         before: {:?}\n\
         limit: 20\n\
         include_context: false\n\
         response_format: \"detailed\"\n\
         sort: \"timestamp\"\n\
         sort_dir: \"asc\"\n\n\
         Then convert every result into this JSON, and output ONLY the JSON \
         with no commentary and no code fence:\n\
         {{\"total\": 0, \"messages\": [{{\"channel_id\": \"...\", \
         \"channel_name\": \"...\", \"ts\": \"...\", \"user_id\": \"...\", \
         \"user_name\": \"...\", \"permalink\": \"...\", \
         \"text\": \"...\"}}]}}\n\n\
         `total` is the result count the tool reported (it prints e.g. \
         \"## Messages (3 results)\"), and `messages` must contain every one \
         of them.\n\
         `channel_name` is the channel's name without the leading #, and \\
         `user_name` is the author's display name.\n\
         Copy each field verbatim from the tool output. Do not summarize, \
         re-word, or omit any message.\n\
         Output {{\"messages\": []}} ONLY if the tool ran and returned no \
         results. If you cannot run the tool for any reason, say so in plain \
         text instead — an empty list is indistinguishable from a quiet \
         channel, so reporting one you didn't earn hides the failure.",
        window.start.timestamp().to_string(),
        window.end.timestamp().to_string(),
    )
}

fn reactions_prompt(tool: &str, channel_id: &str, oldest: &str) -> String {
    format!(
        "Call {tool} with these exact arguments:\n\
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
    fn discover(&self, scope: &Scope, window: &SearchWindow) -> Result<Discovered, SourceError> {
        if window.is_empty() {
            return Ok(Discovered::all(Vec::new()));
        }
        self.discover_with_query(&search_query(scope), window)
    }

    fn user_names(&self, ids: &[String]) -> Result<HashMap<String, String>, SourceError> {
        let mut names = HashMap::new();
        for id in ids {
            // One bad profile shouldn't cost the rest their names.
            if let Ok(name) = self.resolve_user(id) {
                names.insert(id.clone(), name);
            }
        }
        Ok(names)
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
    /// Prime the handle-to-ID cache from somewhere durable. Channel IDs never
    /// change, so a resolution is worth keeping across processes rather than
    /// paying a bridge call for it every invocation.
    pub fn seed_channels(&self, known: impl IntoIterator<Item = (String, String)>) {
        self.channel_ids.borrow_mut().extend(known);
    }

    /// Everything resolved so far, for the caller to persist.
    pub fn known_channels(&self) -> HashMap<String, String> {
        self.channel_ids.borrow().clone()
    }

    pub fn seed_users(&self, known: impl IntoIterator<Item = (String, String)>) {
        self.user_names.borrow_mut().extend(known);
    }

    pub fn known_users(&self) -> HashMap<String, String> {
        self.user_names.borrow().clone()
    }

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

        let tool = &self.tools.search_channels;
        let prompt = format!(
            "Call {tool} with query {name:?}.\n\n\
             Then output ONLY this JSON, no commentary and no code fence:\n\
             {{\"id\": \"...\"}}\n\n\
             Use the id of the channel named exactly {name:?}. If there is no \
             exact match, output {{\"id\": null}}.",
        );
        let id = self
            .attempt(&prompt, &[self.tools.search_channels.as_str()], |raw| {
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
                &reactions_prompt(&self.tools.read_channel, channel_id, &oldest),
                &[self.tools.read_channel.as_str()],
                |raw| parse_reactions(raw).map_err(Retry::Again),
            )
            .map_err(SourceError::Search)?;

        Ok(parsed
            .into_iter()
            .map(|r| (MessageId::new(channel_id, &r.ts), r.names))
            .collect())
    }

    /// Real name for a user ID, cached. One profile call per person ever —
    /// a Slack username isn't a name, and the search results only carry the
    /// username.
    fn resolve_user(&self, user_id: &str) -> Result<String, SourceError> {
        if let Some(known) = self.user_names.borrow().get(user_id) {
            return Ok(known.clone());
        }
        let tool = &self.tools.read_user_profile;
        let prompt = format!(
            "Call {tool} with user_id {user_id:?}.\n\n\
             Then output ONLY this JSON, no commentary and no code fence:\n\
             {{\"id\": {user_id:?}, \"handle\": \"...\", \"name\": \"...\"}}\n\n\
             `name` is the person's real or display name.",
        );
        let identity = self
            .attempt(&prompt, &[self.tools.read_user_profile.as_str()], |raw| {
                parse_identity(raw).map_err(Retry::Again)
            })
            .map_err(SourceError::Search)?;

        let name = if identity.name.trim().is_empty() {
            identity.handle
        } else {
            identity.name
        };
        self.user_names
            .borrow_mut()
            .insert(user_id.to_string(), name.clone());
        Ok(name)
    }

    /// Who the credentials belong to.
    ///
    /// The profile tool defaults to the calling user when given no ID, which
    /// is the only self-identification the MCP server offers. Worth having:
    /// personal mode and the only-my-messages filter both need this ID, and
    /// nobody knows their own Slack ID off-hand.
    pub fn whoami(&self) -> Result<Identity, SourceError> {
        let tool = &self.tools.read_user_profile;
        let prompt = format!(
            "Call {tool} with NO arguments, so it returns the current \
             user's own profile.\n\n\
             Then output ONLY this JSON, no commentary and no code fence:\n\
             {{\"id\": \"...\", \"handle\": \"...\", \"name\": \"...\"}}\n\n\
             `id` is the Slack user ID (starts with U). `handle` is the \
             username or display name. `name` is the real name. Use an empty \
             string for anything the profile doesn't have.",
        );
        self.attempt(&prompt, &[self.tools.read_user_profile.as_str()], |raw| {
            match parse_identity(raw) {
                Ok(identity) if identity.id.starts_with('U') => Ok(identity),
                // A profile without a user ID means the tool didn't really
                // answer; retrying is worth a shot.
                Ok(_) => Err(Retry::Again("profile had no user id".into())),
                Err(error) => Err(Retry::Again(error)),
            }
        })
        .map_err(SourceError::Search)
    }

    /// A thread's parent message and every reply, with reactions.
    pub fn thread(&self, channel_id: &str, parent_ts: &str) -> Result<Vec<Message>, SourceError> {
        let tool = &self.tools.read_thread;
        let prompt = format!(
            "Call {tool} with these exact arguments:\n\
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
            .attempt(&prompt, &[self.tools.read_thread.as_str()], |raw| {
                parse_search(raw).map_err(Retry::Again)
            })
            .map_err(SourceError::Search)?;
        Ok(hits.into_iter().map(SearchHit::into_message).collect())
    }

    /// Who wrote each message of a thread and of the channel after `wanted`,
    /// what reactions the thread's carry, and whom the channel's @mention —
    /// never what any of them says. `thread()` asks for text, which a watch
    /// must not hold: it would be one careless format string from a push.
    ///
    /// Both reads go in one `claude -p`: a second tool call costs a turn, not
    /// another session.
    ///
    /// `wanted` is the message being watched. A read that parsed but leaves
    /// it out is gone: a deleted reply's thread still reads, with no error
    /// code. A model that merely dropped the row looks the same, so the
    /// caller must not end a watch on one such reading.
    pub fn activity(
        &self,
        channel_id: &str,
        root_ts: &str,
        wanted: &str,
    ) -> Result<Activity, ActivityError> {
        let thread_tool = &self.tools.read_thread;
        let channel_tool = &self.tools.read_channel;
        // A reply may be past the thread's first page, and only what follows it
        // counts. `oldest` is exclusive, so start just before it.
        let from = match root_ts == wanted {
            true => String::new(),
            false => crate::domain::slack::just_before(wanted)
                .map(|ts| format!("oldest: {ts:?}\n"))
                .unwrap_or_default(),
        };
        // `oldest` is exclusive, so the channel read is everything after `wanted`.
        let prompt = format!(
            "Call {thread_tool} with these exact arguments:\n\
             channel_id: {channel_id:?}\n\
             message_ts: {root_ts:?}\n\
             {from}\
             limit: {THREAD_PAGE}\n\
             response_format: \"detailed\"\n\n\
             Then call {channel_tool} with these exact arguments:\n\
             channel_id: {channel_id:?}\n\
             oldest: {wanted:?}\n\
             limit: 100\n\
             response_format: \"detailed\"\n\n\
             Then output ONLY this JSON, no commentary and no code fence:\n\
             {{\"messages\": [{{\"ts\": \"...\", \"user_id\": \"...\", \
             \"user_name\": \"...\", \"reactions\": {{\"name\": 1}}}}], \
             \"channel\": [{{\"ts\": \"...\", \"user_id\": \"...\", \
             \"user_name\": \"...\", \"mentions\": [\"U...\"]}}]}}\n\n\
             `messages` is the thread read: the parent message first, then every \
             reply in order. `channel` is every message the channel read \
             returned, in order. `ts` is the Message TS. `user_id` is the ID in \
             parentheses after the author. `user_name` is the author's name as \
             shown before any <email>; never include the email. `reactions` maps \
             each reaction name, exactly as shown and without colons, to its \
             count; use {{}} for a message with none. `mentions` lists the user \
             IDs the message @mentions, written <@U...> in it; use [] for none. \
             Do not include message text.\n\
             If a tool fails, output only its error code.",
        );
        self.attempt(
            &prompt,
            &[thread_tool, channel_tool],
            |raw| match parse_activity(raw) {
                Ok(read) if read.channel.is_none() => {
                    Err(Retry::Again("the read left out the channel".into()))
                }
                Ok(read) if !read.thread.iter().any(|e| e.ts == wanted) => Err(Retry::Fatal(
                    format!("{GONE_MARK}: the read left out {wanted}"),
                )),
                Ok(read) => Ok(read),
                Err(error) => Err(unparsed(raw, error)),
            },
        )
        .map_err(|error| match error.starts_with(GONE_MARK) {
            true => ActivityError::Gone(error),
            false => ActivityError::Failed(error),
        })
    }

    /// Who wrote each top-level message of a channel after `oldest`
    /// (exclusive) — never what it says. With no `oldest`, only the newest
    /// message, which is all a baseline needs.
    ///
    /// One page: the tool returns newest first, so a busier stretch loses its
    /// earliest messages and the read says `more` rather than miscounting.
    pub fn channel_messages(
        &self,
        channel_id: &str,
        oldest: Option<&str>,
    ) -> Result<ChannelRead, ActivityError> {
        let tool = &self.tools.read_channel;
        let (bound, limit) = match oldest {
            Some(ts) => (format!("oldest: {ts:?}\n"), CHANNEL_PAGE),
            None => (String::new(), 1),
        };
        let prompt = format!(
            "Call {tool} with these exact arguments:\n\
             channel_id: {channel_id:?}\n\
             {bound}\
             limit: {limit}\n\
             response_format: \"detailed\"\n\n\
             Then output ONLY this JSON, no commentary and no code fence:\n\
             {{\"channel\": [{{\"ts\": \"...\", \"user_id\": \"...\", \
             \"user_name\": \"...\"}}], \"more\": false}}\n\n\
             `channel` is every message the read returned, in order; [] for \
             none. `ts` is the Message TS. `user_id` is the ID in parentheses \
             after the author. `user_name` is the author's name as shown before \
             any <email>; never include the email. `more` is true when the tool \
             says more messages are available. Do not include message text.\n\
             If the tool fails, output only its error code.",
        );
        self.attempt(&prompt, &[tool], |raw| {
            parse_channel_read(raw)
                .map(|mut read| {
                    // A full page is more whether or not the model said so.
                    read.more |= read.channel.len() >= limit;
                    read
                })
                .map_err(|error| unparsed(raw, error))
        })
        .map_err(|error| match error.starts_with(GONE_MARK) {
            true => ActivityError::Gone(error),
            false => ActivityError::Failed(error),
        })
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
        let tool = &self.tools.send;
        let prompt = format!(
            "Call {tool} with these exact arguments:\n\
             channel_id: {channel_id:?}\n\
             {threading}\
             message: {text:?}\n\n\
             Then output ONLY the word OK, or the error code if it failed.",
        );
        self.attempt(&prompt, &[self.tools.send.as_str()], |raw| {
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
        Ok(self.discover_with_query(query, window)?.messages)
    }

    fn discover_with_query(
        &self,
        query: &str,
        window: &SearchWindow,
    ) -> Result<Discovered, SourceError> {
        let hits = self
            .attempt(
                &search_prompt(&self.tools.search, query, window),
                &[self.tools.search.as_str()],
                |raw| parse_search(raw).map_err(Retry::Again),
            )
            .map_err(SourceError::Search)?;

        // A full page means Slack had more to give. Saying so lets the caller
        // advance its mark only as far as it actually covered.
        let complete = hits.len() < PAGE_LIMIT;
        Ok(Discovered {
            messages: hits.into_iter().map(SearchHit::into_message).collect(),
            complete,
        })
    }
}

impl<R: Runner> ReactionSink for SlackClaude<R> {
    fn add(&self, target: &MessageId, emoji: &ReactionName) -> Result<(), SinkError> {
        let tool = &self.tools.react;
        let prompt = format!(
            "Call {tool} with these exact arguments:\n\
             channel_id: {:?}\n\
             message_ts: {:?}\n\
             emoji: {:?}\n\n\
             Then output ONLY the word OK, or the error code if the call failed.",
            target.channel,
            target.ts,
            emoji.as_str(),
        );
        self.attempt(&prompt, &[self.tools.react.as_str()], |raw| {
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
        fn run(
            &self,
            _prompt: &str,
            _tools: &[&str],
            _attempt: usize,
        ) -> Result<String, RunnerError> {
            *self.calls.borrow_mut() += 1;
            Ok(self.replies.borrow_mut().pop().unwrap_or_default())
        }
    }

    #[test]
    fn a_bridge_hiccup_is_retried() {
        // Observed in a live run: the model claimed it had no Slack tools,
        // then worked fine on the next call.
        let runner = ScriptedRunner::new(&["I don't have access to any Slack MCP tools.", "OK"]);
        let slack = SlackClaude::new(runner).without_delay();
        let result = slack.add(&MessageId::new("C1", "1.0"), &ReactionName::new("merged"));
        assert!(result.is_ok());
    }

    #[test]
    fn a_refusal_slack_actually_made_is_not_retried() {
        // Retrying a missing emoji just burns time and money.
        let runner = ScriptedRunner::new(&["execution_failed: invalid_name", "OK"]);
        let slack = SlackClaude::new(runner).without_delay();
        let error = slack
            .add(&MessageId::new("C1", "1.0"), &ReactionName::new("nope"))
            .unwrap_err();
        assert!(matches!(error, SinkError::InvalidEmoji(_)), "{error:?}");
    }

    #[test]
    fn a_bridge_timeout_is_not_retried() {
        // Retries are for the hiccup that answers in seconds; a hang costs
        // the whole timeout again on every try.
        struct Hangs(RefCell<usize>);
        impl Runner for Hangs {
            fn run(&self, _: &str, _: &[&str], _: usize) -> Result<String, RunnerError> {
                *self.0.borrow_mut() += 1;
                Err(RunnerError::TimedOut {
                    command: "claude".into(),
                    timeout: Duration::from_secs(180),
                })
            }
        }
        let slack = SlackClaude::new(Hangs(RefCell::new(0))).without_delay();
        let error = slack
            .add(&MessageId::new("C1", "1.0"), &ReactionName::new("merged"))
            .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
        assert_eq!(*slack.runner.0.borrow(), 1);
    }

    #[test]
    fn giving_up_reports_the_last_failure() {
        let runner = ScriptedRunner::new(&["nonsense", "more nonsense", "still nonsense"]);
        let slack = SlackClaude::new(runner).without_delay();
        let error = slack
            .add(&MessageId::new("C1", "1.0"), &ReactionName::new("merged"))
            .unwrap_err();
        assert!(error.to_string().contains("3 attempts"), "{error}");
    }

    #[test]
    fn queries_use_scope_modifiers_only() {
        assert_eq!(
            search_query(&Scope::Personal("U012ABC345".into())),
            "from:<@U012ABC345>"
        );
        assert_eq!(
            search_query(&Scope::Channel("#inkling".into())),
            "in:#inkling"
        );
        // A channel configured without the hash still works.
        assert_eq!(
            search_query(&Scope::Channel("inkling".into())),
            "in:#inkling"
        );
    }

    #[test]
    fn a_channel_id_uses_slacks_other_spelling() {
        // `in:#C012XYZ6789` is accepted and matches nothing, so getting this
        // wrong looks like an empty channel rather than an error.
        assert_eq!(
            search_query(&Scope::Channel("C012XYZ6789".into())),
            "in:<#C012XYZ6789>"
        );
    }

    #[test]
    fn the_search_prompt_carries_unix_timestamps() {
        let window = SearchWindow {
            start: "2026-08-12T21:00:00Z".parse().unwrap(),
            end: "2026-08-12T21:30:00Z".parse().unwrap(),
        };
        let prompt = search_prompt("search_tool", "in:#inkling", &window);
        assert!(prompt.contains("1786568400"), "{prompt}");
        assert!(prompt.contains("1786570200"), "{prompt}");
    }

    #[test]
    fn a_seeded_handle_resolves_without_a_bridge_call() {
        // Empty script: any bridge call would return "" and fail to parse.
        let slack = SlackClaude::new(ScriptedRunner::new(&[]));
        slack.seed_channels([("eng".to_string(), "C012XYZ6789".to_string())]);

        assert_eq!(slack.resolve_channel("#eng").unwrap(), "C012XYZ6789");
        assert_eq!(*slack.runner.calls.borrow(), 0);
    }

    #[test]
    fn resolutions_are_exposed_for_persisting() {
        let slack = SlackClaude::new(ScriptedRunner::new(&[r#"{"id":"C012XYZ6789"}"#]));
        slack.resolve_channel("eng").unwrap();
        assert_eq!(
            slack.known_channels().get("eng").map(String::as_str),
            Some("C012XYZ6789")
        );
    }

    #[test]
    fn channel_ids_are_told_apart_from_handles_without_a_lookup() {
        // Resolving a handle costs a bridge call, so an ID must never take
        // that path.
        assert!(looks_like_channel_id("C012XYZ6789"));
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
