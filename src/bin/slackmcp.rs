//! `slackmcp` — a thin CLI over the Slack MCP server, via a `claude`
//! subprocess.
//!
//! Inkling uses this bridge as a library; this binary is the same code with a
//! command line on it, so the transport can be exercised by hand while
//! debugging and stays useful on its own. It prints JSON, so it composes with
//! `jq` and with anything else that wants Slack data without Slack
//! credentials.
//!
//! Usage:
//!   slackmcp search <query> [--after <unix>] [--before <unix>]
//!   slackmcp reactions <channel_id> [--oldest <ts>]
//!   slackmcp react <channel_id> <ts> <emoji>

use chrono::{DateTime, TimeDelta, Utc};
use inkling::adapters::slack_claude::{ClaudeRunner, SlackClaude};
use inkling::domain::model::{MessageId, ReactionName};
use inkling::domain::window::SearchWindow;
use inkling::ports::{MessageSource, ReactionSink, Scope};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(error) = run(&args) {
        eprintln!("slackmcp: {error}");
        std::process::exit(1);
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let slack = SlackClaude::new(ClaudeRunner::default());
    match args.first().map(String::as_str) {
        Some("search") => {
            let query = args.get(1).ok_or("search needs a query")?;
            let window = SearchWindow {
                start: flag_time(args, "--after")
                    .unwrap_or_else(|| Utc::now() - TimeDelta::days(1)),
                end: flag_time(args, "--before").unwrap_or_else(Utc::now),
            };
            // A raw query passes straight through, so every Slack search
            // modifier works without this CLI needing to know about them.
            let messages = slack
                .discover(&Scope::Channel(query.clone()), &window)
                .or_else(|_| slack.discover_personal(query, &window))
                .map_err(|e| e.to_string())?;
            for message in messages {
                println!(
                    "{}\t{}\t{}",
                    message.id,
                    message.author,
                    message.text.replace('\n', " ")
                );
            }
        }
        Some("reactions") => {
            let channel = args.get(1).ok_or("reactions needs a channel id")?;
            let oldest = args.get(2).cloned().unwrap_or_else(|| "0".to_string());
            let id = MessageId::new(channel, oldest);
            let found = slack.reactions(&[id]).map_err(|e| e.to_string())?;
            for (id, names) in found {
                let names: Vec<_> = names.iter().map(|r| r.to_string()).collect();
                println!("{id}\t{}", names.join(","));
            }
        }
        Some("react") => {
            let channel = args.get(1).ok_or("react needs a channel id")?;
            let ts = args.get(2).ok_or("react needs a message ts")?;
            let emoji = args.get(3).ok_or("react needs an emoji")?;
            slack
                .add(&MessageId::new(channel, ts), &ReactionName::new(emoji))
                .map_err(|e| e.to_string())?;
            println!("ok");
        }
        _ => {
            eprintln!(
                "usage:\n  \
                 slackmcp search <query> [--after <unix>] [--before <unix>]\n  \
                 slackmcp reactions <channel_id> [oldest_ts]\n  \
                 slackmcp react <channel_id> <ts> <emoji>"
            );
            return Err("unknown command".into());
        }
    }
    Ok(())
}

fn flag_time(args: &[String], flag: &str) -> Option<DateTime<Utc>> {
    let position = args.iter().position(|a| a == flag)?;
    let raw = args.get(position + 1)?;
    DateTime::from_timestamp(raw.parse().ok()?, 0)
}
