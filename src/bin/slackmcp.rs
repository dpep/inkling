//! `slackmcp` — a thin CLI over the Slack MCP server, via a `claude`
//! subprocess.
//!
//! Inkling uses this bridge as a library; this binary is the same code with a
//! command line on it, so the transport is debuggable by hand and useful on
//! its own — Slack data without Slack credentials.
//!
//! Output defaults to tab-separated for humans and pipes; `--json` and
//! `--ndjson` make it machine- and agent-friendly.

use chrono::{DateTime, TimeDelta, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};
use inkling::adapters::slack_claude::{ClaudeRunner, SlackClaude};
use inkling::domain::model::{Message, MessageId, ReactionName};
use inkling::domain::window::SearchWindow;
use inkling::ports::{MessageSource, ReactionSink, Scope};
use serde::Serialize;

#[derive(Parser)]
#[command(
    name = "slackmcp",
    version,
    about = "Read and annotate Slack through the Slack MCP server.",
    long_about = "Drives the Slack MCP server through a `claude` subprocess, so \
Slack is reachable without Slack CLI credentials.\n\n\
Every call allowlists only the one MCP tool it needs, so this cannot post \
messages regardless of what any Slack content asks for.\n\n\
Channels may be given as an ID (C0BPR3AEAG3) or a handle (#eng, eng); handles \
cost one extra lookup to resolve."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    #[command(flatten)]
    format: FormatArgs,

    /// Model to drive the bridge with. Small is correct here — it executes a
    /// tool, it doesn't decide anything.
    #[arg(long, default_value = "haiku", global = true)]
    model: String,
}

#[derive(Args)]
struct FormatArgs {
    /// Emit a JSON array.
    #[arg(long, global = true, conflicts_with = "ndjson")]
    json: bool,
    /// Emit one JSON object per line.
    #[arg(long, global = true)]
    ndjson: bool,
}

impl FormatArgs {
    fn resolve(&self) -> Format {
        match (self.json, self.ndjson) {
            (true, _) => Format::Json,
            (_, true) => Format::Ndjson,
            _ => Format::Text,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Text,
    Json,
    Ndjson,
}

#[derive(Subcommand)]
enum Command {
    /// Find messages. QUERY takes Slack search syntax, or a bare channel
    /// handle/ID as a shorthand for `in:#channel`.
    Search {
        /// e.g. "#eng", "C0BPR3AEAG3", "from:<@U123>", "in:#eng has:link"
        query: String,
        /// Only messages at or after this Unix timestamp.
        #[arg(long)]
        after: Option<i64>,
        /// Only messages at or before this Unix timestamp.
        #[arg(long)]
        before: Option<i64>,
        /// Look back this many hours when --after is omitted.
        #[arg(long, default_value_t = 24)]
        hours: i64,
    },
    /// Show reactions on a channel's recent messages.
    Reactions {
        /// Channel ID or handle.
        channel: String,
        /// Only messages at or after this Slack timestamp.
        #[arg(long, default_value = "0")]
        since: String,
    },
    /// Add one or more reactions to a message.
    React {
        /// Channel ID or handle.
        channel: String,
        /// Slack message timestamp, e.g. 1786596050.245879
        ts: String,
        /// Emoji names, with or without colons. An emoji the workspace
        /// doesn't have fails with invalid_name; Slack exposes no way to
        /// list the available ones.
        #[arg(required = true)]
        emoji: Vec<String>,
    },
    /// Read a thread: the parent message and every reply.
    Thread {
        /// Channel ID or handle.
        channel: String,
        /// Timestamp of the parent message.
        ts: String,
    },
    /// Post a message, or reply in a thread.
    ///
    /// The only subcommand that writes anything beyond a reaction. Inkling's
    /// own run loop never reaches this.
    Post {
        /// Channel ID or handle.
        channel: String,
        /// Message text.
        text: String,
        /// Reply to this thread rather than posting to the channel.
        #[arg(long)]
        thread: Option<String>,
    },
    /// Resolve a channel handle to its ID.
    Channel {
        /// Channel handle, e.g. "#eng" or "eng".
        name: String,
    },
}

#[derive(Serialize)]
struct MessageOut {
    channel_id: String,
    ts: String,
    user_id: String,
    permalink: String,
    text: String,
}

impl From<Message> for MessageOut {
    fn from(m: Message) -> Self {
        Self {
            channel_id: m.id.channel,
            ts: m.id.ts,
            user_id: m.author,
            permalink: m.permalink,
            text: m.text,
        }
    }
}

#[derive(Serialize)]
struct ReactionsOut {
    channel_id: String,
    ts: String,
    reactions: Vec<String>,
}

#[derive(Serialize)]
struct ChannelOut {
    name: String,
    id: String,
}

#[derive(Serialize)]
struct ReactOut {
    channel_id: String,
    ts: String,
    emoji: String,
    added: bool,
}

fn main() {
    let cli = Cli::parse();
    if let Err(error) = run(&cli) {
        eprintln!("slackmcp: {error}");
        std::process::exit(1);
    }
}

fn run(cli: &Cli) -> Result<(), String> {
    let runner = ClaudeRunner {
        model: Some(cli.model.clone()),
        ..ClaudeRunner::default()
    };
    let slack = SlackClaude::new(runner);
    let format = cli.format.resolve();

    match &cli.command {
        Command::Search {
            query,
            after,
            before,
            hours,
        } => {
            let end = before
                .and_then(|t| DateTime::from_timestamp(t, 0))
                .unwrap_or_else(Utc::now);
            let start = after
                .and_then(|t| DateTime::from_timestamp(t, 0))
                .unwrap_or_else(|| end - TimeDelta::hours(*hours));

            let window = SearchWindow { start, end };
            let messages = search(&slack, query, &window)?;
            let rows: Vec<MessageOut> = messages.into_iter().map(Into::into).collect();
            emit(format, &rows, |r| {
                vec![
                    format!("{}:{}", r.channel_id, r.ts),
                    r.user_id.clone(),
                    r.text.replace('\n', " "),
                ]
            })
        }
        Command::Reactions { channel, since } => {
            let id = slack.resolve_channel(channel).map_err(|e| e.to_string())?;
            let found = slack
                .channel_reactions(&id, since)
                .map_err(|e| e.to_string())?;

            let mut rows: Vec<ReactionsOut> = found
                .into_iter()
                .map(|(id, names)| ReactionsOut {
                    channel_id: id.channel,
                    ts: id.ts,
                    reactions: names.iter().map(ToString::to_string).collect(),
                })
                .collect();
            rows.sort_by(|a, b| a.ts.cmp(&b.ts));

            emit(format, &rows, |r| {
                vec![format!("{}:{}", r.channel_id, r.ts), r.reactions.join(",")]
            })
        }
        Command::React { channel, ts, emoji } => {
            let id = slack.resolve_channel(channel).map_err(|e| e.to_string())?;
            let target = MessageId::new(&id, ts);

            // One bad emoji shouldn't discard the ones that worked, so each is
            // reported on its own row.
            let mut rows = Vec::new();
            let mut failures = Vec::new();
            for name in emoji {
                let name = ReactionName::new(name);
                match slack.add(&target, &name) {
                    Ok(()) => rows.push(ReactOut {
                        channel_id: id.clone(),
                        ts: ts.clone(),
                        emoji: name.to_string(),
                        added: true,
                    }),
                    Err(error) => failures.push(format!(":{name}: {error}")),
                }
            }
            emit(format, &rows, |r| {
                vec![format!("{}:{}", r.channel_id, r.ts), r.emoji.clone()]
            })?;
            if failures.is_empty() {
                Ok(())
            } else {
                Err(failures.join("; "))
            }
        }
        Command::Thread { channel, ts } => {
            let id = slack.resolve_channel(channel).map_err(|e| e.to_string())?;
            let messages = slack.thread(&id, ts).map_err(|e| e.to_string())?;
            let rows: Vec<MessageOut> = messages.into_iter().map(Into::into).collect();
            emit(format, &rows, |r| {
                vec![
                    format!("{}:{}", r.channel_id, r.ts),
                    r.user_id.clone(),
                    r.text.replace('\n', " "),
                ]
            })
        }
        Command::Post {
            channel,
            text,
            thread,
        } => {
            let id = slack.resolve_channel(channel).map_err(|e| e.to_string())?;
            slack
                .post(&id, text, thread.as_deref())
                .map_err(|e| e.to_string())?;
            println!("ok");
            Ok(())
        }
        Command::Channel { name } => {
            let id = slack.resolve_channel(name).map_err(|e| e.to_string())?;
            let rows = vec![ChannelOut {
                name: name.trim_start_matches('#').to_string(),
                id,
            }];
            emit(format, &rows, |r| vec![r.id.clone(), r.name.clone()])
        }
    }
}

/// Search takes a handle directly — no resolution needed, which also saves a
/// bridge call. Only reactions and react need a real channel ID.
///
/// A bare handle or ID is the common case, so it's the shorthand; anything
/// containing a Slack search modifier passes through untouched.
fn search<R: inkling::adapters::slack_claude::Runner>(
    slack: &SlackClaude<R>,
    query: &str,
    window: &SearchWindow,
) -> Result<Vec<Message>, String> {
    if query.contains(':') {
        return slack.discover_raw(query, window).map_err(|e| e.to_string());
    }
    slack
        .discover(&Scope::Channel(query.to_string()), window)
        .map_err(|e| e.to_string())
}

fn emit<T: Serialize>(
    format: Format,
    rows: &[T],
    columns: impl Fn(&T) -> Vec<String>,
) -> Result<(), String> {
    match format {
        Format::Json => {
            let json = serde_json::to_string_pretty(rows).map_err(|e| e.to_string())?;
            println!("{json}");
        }
        Format::Ndjson => {
            for row in rows {
                let json = serde_json::to_string(row).map_err(|e| e.to_string())?;
                println!("{json}");
            }
        }
        Format::Text => {
            for row in rows {
                println!("{}", columns(row).join("\t"));
            }
        }
    }
    Ok(())
}
