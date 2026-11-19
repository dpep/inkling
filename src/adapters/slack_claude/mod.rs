//! Slack, reached by driving the Slack MCP server through a `claude`
//! subprocess.
//!
//! This exists because the Slack CLI isn't usable in the current environment.
//! It is a transport, not a decision-maker: the model is asked to call one
//! or two read tools and hand back who wrote what and when, never what it says.

mod parse;
mod runner;

pub use parse::{
    Activity, ChannelRead, Identity, ThreadEntry, parse_activity, parse_channel_id,
    parse_channel_read, parse_identity,
};
#[cfg(test)]
pub use parse::{
    REAL_CHANNEL_BASELINE, REAL_CHANNEL_READ, REAL_DM_ACTIVITY, REAL_EMPTY_CHANNEL, REAL_THREAD,
};
pub use runner::{BRIDGE_ENV, ClaudeRunner, Runner, RunnerError, extract_result};

use crate::config::SlackMcp;
use crate::ports::SourceError;
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

/// The account-linked connector this has always used. Still the default, so
/// an absent `[slack.mcp]` changes nothing.
const DEFAULT_SERVER: &str = "claude_ai_Slack";

/// Fully-qualified tool names, by role.
///
/// Names are allowlisted per call, so a prompt-injection attempt in a Slack
/// message has nothing to reach for. `react` and `send` are never called:
/// they name the session's own tool calls, which the post-tool hook watches.
#[derive(Debug, Clone)]
pub struct ToolNames {
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

/// Build the bridge from config.
pub fn from_config(
    config: &crate::config::Config,
) -> Result<SlackClaude<ClaudeRunner>, crate::config::ConfigError> {
    let mcp = config.slack.mcp.as_ref();
    let runner = ClaudeRunner {
        mcp_config: mcp.map(|m| m.config_json()).transpose()?,
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

/// `slack_read_channel`'s maximum page.
pub const CHANNEL_PAGE: usize = 100;

/// One `slack_read_thread` page. It lists oldest first, so a thread longer
/// than this loses its newest replies, the ones that are news. Not the tool's
/// 1000: the model writes a row back per reply, and a page that size can't
/// be written inside the bridge's timeout, so the thread would never read.
const THREAD_PAGE: usize = 100;

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
        }
    }

    #[cfg(test)]
    pub(crate) fn without_delay(mut self) -> Self {
        self.retry_delay = Duration::ZERO;
        self
    }

    /// Run a prompt and interpret the reply, retrying when the reply doesn't
    /// make sense. Transport errors are retried too, except a timeout: a hang
    /// costs the whole timeout again; nor is a server stopping. A typed
    /// refusal from Slack — a deleted message, no such channel — is not,
    /// since repeating it would just cost time.
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
                Err(error @ (RunnerError::TimedOut { .. } | RunnerError::Stopping)) => {
                    return Err(error.to_string());
                }
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

impl<R: Runner> SlackClaude<R> {
    /// Prime the handle-to-ID cache from somewhere durable. Channel IDs never
    /// change, so a resolution is worth keeping across processes rather than
    /// paying a bridge call for it every invocation.
    pub fn seed_channels(&self, known: impl IntoIterator<Item = (String, String)>) {
        self.channel_ids.borrow_mut().extend(known);
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

    /// Who the credentials belong to.
    ///
    /// The profile tool defaults to the calling user when given no ID, which
    /// is the only self-identification the MCP server offers. Worth having:
    /// a channel message counts as a reply only when it mentions this ID, and
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

    /// Who wrote each message of a thread and of the channel after `wanted`,
    /// what reactions the thread's carry, and whom the channel's @mention —
    /// never what any of them says. `thread()` asks for text, which a watch
    /// must not hold: it would be one careless format string from a push.
    ///
    /// Both reads go in one `claude -p`: a second tool call costs a turn, not
    /// another session.
    ///
    /// `wanted` is the message being watched. A thread that read but leaves
    /// it out is gone: a deleted reply's thread still reads, with no error
    /// code. A thread with no rows read nothing, and is retried. A model that merely dropped the row looks the same, so the
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
                // Gone only beside rows that did read: an empty thread is a
                // model that read nothing (or echoed the template).
                Ok(read) if read.thread.is_empty() => {
                    Err(Retry::Again("the read found no thread".into()))
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
        let runner = ScriptedRunner::new(&[
            "I don't have access to any Slack MCP tools.",
            r#"{"id":"U012ABC345","handle":"me","name":"Me"}"#,
        ]);
        let slack = SlackClaude::new(runner).without_delay();
        assert_eq!(slack.whoami().unwrap().id, "U012ABC345");
    }

    #[test]
    fn a_refusal_slack_actually_made_is_not_retried() {
        // Asking again for a channel that doesn't exist just burns time and money.
        let runner = ScriptedRunner::new(&[r#"{"id": null}"#, r#"{"id":"C012XYZ6789"}"#]);
        let slack = SlackClaude::new(runner).without_delay();
        let error = slack.resolve_channel("#nope").unwrap_err();
        assert!(error.to_string().contains("no channel named"), "{error}");
        assert_eq!(*slack.runner.calls.borrow(), 1);
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
        let error = slack.whoami().unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
        assert_eq!(*slack.runner.0.borrow(), 1);
    }

    #[test]
    fn a_stopping_server_is_not_retried() {
        struct Stopping(RefCell<usize>);
        impl Runner for Stopping {
            fn run(&self, _: &str, _: &[&str], _: usize) -> Result<String, RunnerError> {
                *self.0.borrow_mut() += 1;
                Err(RunnerError::Stopping)
            }
        }
        let slack = SlackClaude::new(Stopping(RefCell::new(0))).without_delay();
        slack.whoami().unwrap_err();
        assert_eq!(*slack.runner.0.borrow(), 1);
    }

    #[test]
    fn giving_up_reports_the_last_failure() {
        let runner = ScriptedRunner::new(&["nonsense", "more nonsense", "still nonsense"]);
        let slack = SlackClaude::new(runner).without_delay();
        let error = slack.whoami().unwrap_err();
        assert!(error.to_string().contains("3 attempts"), "{error}");
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
}
