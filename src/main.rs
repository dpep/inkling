//! `inkling` — annotate Slack messages with the state of what they link to.

use chrono::{DateTime, TimeDelta, Utc};
use clap::{Args, Parser, Subcommand};
use inkling::adapters::github::GitHubProvider;
use inkling::adapters::slack_claude;
use inkling::adapters::store_json::{JsonStore, Store};
use inkling::app::{Report, Run};
use inkling::config::Config;
use inkling::config_edit;
use inkling::obs;
use inkling::ports::SystemClock;
use inkling::secret::Secret;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "inkling",
    version,
    about = "Annotate Slack messages with the current state of what they link to.",
    long_about = "Finds GitHub pull-request links in Slack messages and keeps the \
message's reactions reflecting where each PR is.\n\n\
Built to run from cron. Start with `inkling run --dry-run` to see what it would do."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    #[command(flatten)]
    common: Common,
}

#[derive(Args)]
struct Common {
    /// Config file. Defaults to $XDG_CONFIG_HOME/inkling/config.toml
    #[arg(long, short, global = true)]
    config: Option<PathBuf>,
    /// State file. Defaults to $XDG_STATE_HOME/inkling/state.json
    #[arg(long, global = true)]
    state: Option<PathBuf>,
    /// More detail. -v adds per-message decisions, -vv adds message text.
    #[arg(long, short, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
    /// Structured logs, for a collector rather than a person.
    #[arg(long, global = true)]
    log_json: bool,
    /// Where daily log files go. Defaults to $XDG_STATE_HOME/inkling/logs;
    /// "off" disables file logging.
    #[arg(long, global = true)]
    log_dir: Option<String>,
    /// Delete log files older than this many days.
    #[arg(long, global = true, default_value_t = 14)]
    log_keep_days: usize,
}

#[derive(Subcommand)]
enum Command {
    /// Discover new messages and bring tracked ones up to date.
    Run {
        /// Only search these channels, ignoring the configured scope. Handles
        /// or IDs. Given any, personal mode is off for this run — asking for
        /// a channel and getting your DMs too would be surprising.
        channels: Vec<String>,
        /// Report what would happen, changing nothing in Slack or on disk.
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Widen the search window to this many hours back, for a one-off
        /// catch-up. Narrowing it will not advance the watermark.
        #[arg(long)]
        since_hours: Option<i64>,
        /// Emit the run report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show the search window, tracked records, and credential status.
    Status,
    /// Inspect or create the config file.
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
    /// Read the log files.
    Logs {
        /// How many lines to show.
        #[arg(long, short = 'n', default_value_t = 40)]
        lines: usize,
        /// Only lines containing this, case-insensitively.
        #[arg(long, short = 'g')]
        grep: Option<String>,
        /// Keep printing as new lines arrive.
        #[arg(long, short)]
        follow: bool,
        /// Print the log directory and exit.
        #[arg(long)]
        path: bool,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print the effective config — defaults plus whatever the file changes.
    Show,
    /// Print where the config file would be read from.
    Path,
    /// Write a starter config file.
    Init {
        /// Overwrite an existing config.
        #[arg(long)]
        force: bool,
    },
    /// Print one key's value, as written in the file.
    Get {
        /// Dotted path, e.g. slack.personal or status.merged.canonical
        key: String,
    },
    /// Set one key. An empty value removes it.
    Set {
        /// Dotted path, e.g. slack.personal or status.merged.canonical
        key: String,
        /// Type is inferred: true/false, whole numbers, comma-separated
        /// lists, otherwise a string. Pass "" to remove the key.
        value: String,
    },
}

/// A working config out of the box: personal mode needs nothing else, since
/// the user ID resolves itself. Emoji default to ones every workspace has —
/// `:merged:` and `:shipit:` are common but not universal, and adding one the
/// workspace lacks fails with invalid_name.
const STARTER_CONFIG: &str = r##"# Inkling — annotates Slack messages with the state of what they link to.
# Every field here has a default; this file is a starting point, not a schema.

[slack]
# Find pull-request links in your own posts, wherever you post them.
personal = true
# ...and/or watch specific channels. Handles, not IDs.
# channels = ["#eng-prs"]
# user_id resolves from your credentials on first run. `slackmcp whoami` prints it.

[filters]
# Only annotate messages you wrote, however they were found. Worth keeping on
# until the behavior is boring: every reaction it leaves is then on a post of
# yours, which is both a small blast radius and legible feedback.
only_my_messages = true

[github]
token_command = "gh auth token"   # or token_env = "GITHUB_TOKEN"

# Only states listed here are ever signaled — open, draft, and closed stay
# silent unless you add them. `canonical` is what Inkling adds; `aliases` are
# what count as someone having already said it, so it stays quiet.
[status.approved]
canonical = "white_check_mark"
aliases = ["heavy_check_mark", "lgtm", "approved"]

[status.merged]
canonical = "rocket"              # swap for "merged" if your workspace has it
aliases = ["merged", "shipit", "ship", "done"]
"##;

fn main() {
    let cli = Cli::parse();

    let directory = match cli.common.log_dir.as_deref() {
        Some("off") => None,
        Some(path) => Some(PathBuf::from(path)),
        None => Some(obs::default_log_directory()),
    };
    // Sweep first: max_log_files only prunes on rotation, so a job that
    // stopped running would otherwise keep its files forever.
    if let Some(dir) = &directory {
        let _ = obs::prune(dir, cli.common.log_keep_days);
    }
    let _logging = obs::init(&obs::Options {
        format: if cli.common.log_json {
            obs::Format::Json
        } else {
            obs::Format::Text
        },
        directory,
        keep_days: cli.common.log_keep_days,
        verbosity: cli.common.verbose,
        // The run summary is printed on stdout; the terminal layer would
        // otherwise say everything twice.
        quiet: cli.common.verbose == 0,
    });

    let code = match run(&cli) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("inkling: {error}");
            2
        }
    };
    // process::exit skips destructors, which would strand whatever the
    // background log writer still holds — including the run summary and any
    // error that explains the exit code.
    drop(_logging);
    std::process::exit(code);
}

fn config_path(common: &Common) -> PathBuf {
    common.config.clone().unwrap_or_else(|| {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(std::env::temp_dir);
        base.join("inkling").join("config.toml")
    })
}

fn run(cli: &Cli) -> Result<i32, String> {
    let path = config_path(&cli.common);

    // Before the load, since these two exist for when there's no file yet.
    match &cli.command {
        Command::Config {
            action: Some(ConfigAction::Init { force }),
        } => return init(&path, *force),
        Command::Config {
            action: Some(ConfigAction::Path),
        } => {
            println!("{}", path.display());
            return Ok(0);
        }
        // Reading logs shouldn't depend on the config being loadable — a
        // broken config is exactly when you want to read them.
        Command::Logs {
            lines,
            grep,
            follow,
            path: show_path,
        } => {
            let directory = match cli.common.log_dir.as_deref() {
                Some("off") | None => obs::default_log_directory(),
                Some(dir) => PathBuf::from(dir),
            };
            if *show_path {
                println!("{}", directory.display());
                return Ok(0);
            }
            return logs(&directory, *lines, grep.as_deref(), *follow);
        }
        Command::Config {
            action: Some(ConfigAction::Get { key }),
        } => {
            let document = config_edit::load(&path).map_err(|e| e.to_string())?;
            println!(
                "{}",
                config_edit::get(&document, key).map_err(|e| format!(
                    "{e}\n  the file only holds what you've changed; \
                     `inkling config` shows effective values"
                ))?
            );
            return Ok(0);
        }
        Command::Config {
            action: Some(ConfigAction::Set { key, value }),
        } => {
            let mut document = config_edit::load(&path).map_err(|e| e.to_string())?;
            config_edit::set(&mut document, key, value).map_err(|e| e.to_string())?;
            config_edit::save(&path, &document).map_err(|e| e.to_string())?;

            if value.is_empty() {
                println!("removed {key} — now using the default");
            } else {
                println!(
                    "{key} = {}",
                    config_edit::get(&document, key).unwrap_or_default()
                );
            }
            return Ok(0);
        }
        _ => {}
    }

    // A missing file is not an error: every field has a working default.
    let config = Config::load(&path).map_err(|e| e.to_string())?;
    let store = JsonStore::new(
        cli.common
            .state
            .clone()
            .unwrap_or_else(JsonStore::default_path),
    );

    match &cli.command {
        // Handled above, before the config load they don't need.
        Command::Logs { .. } => unreachable!(),
        // Show is the default; Init and Path returned above.
        Command::Config { .. } => {
            let absent = if path.exists() {
                ""
            } else {
                "   (absent — using defaults)"
            };
            println!("config     {}{absent}", tildify(&path));
            println!("state      {}", tildify(store.path()));
            println!("logs       {}", tildify(&obs::default_log_directory()));

            println!();
            println!("personal   {}", config.slack.personal);
            println!(
                "channels   {}",
                if config.slack.channels.is_empty() {
                    "none".to_string()
                } else {
                    config.slack.channels.join(", ")
                }
            );
            println!("mine only  {}", config.filters.only_my_messages);
            println!("github     {}", credentials(&config));

            // Canonical emoji vary in width, so the "also" column is padded to
            // the widest one rather than a guess that breaks on a long name.
            let drawn: Vec<_> = inkling::domain::model::State::ALL
                .into_iter()
                .filter_map(|state| config.canonical_for(state).map(|e| (state, e)))
                .collect();
            let widest = drawn
                .iter()
                .map(|(_, emoji)| emoji.as_str().len())
                .max()
                .unwrap_or(0);

            if !drawn.is_empty() {
                println!();
            }
            for (state, emoji) in drawn {
                // The canonical is in the evidence list too; repeating it in
                // "also" would suggest it's a second, different emoji.
                let also: Vec<String> = config
                    .evidence_for(state)
                    .iter()
                    .filter(|e| **e != emoji)
                    .map(|e| format!(":{e}:"))
                    .collect();
                let canonical = format!(":{emoji}:");
                let padded = format!("{canonical:<width$}", width = widest + 2);
                match also.is_empty() {
                    true => println!("{state:<10} {padded}"),
                    false => println!("{state:<10} {padded}  also {}", also.join(" ")),
                }
            }
            Ok(0)
        }
        Command::Status => {
            let snapshot = store.load().map_err(|e| e.to_string())?;
            let now = Utc::now();
            let window = inkling::domain::window::next_window(
                &config.window,
                snapshot.run.last_successful_discovery_at,
                now,
            );
            let local = |t: DateTime<Utc>| {
                t.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            };
            println!(
                "last run    {}",
                match snapshot.run.last_run_outcome {
                    Some(outcome) => format!("{outcome:?}").to_lowercase(),
                    None => "never".into(),
                }
            );
            println!(
                "last search {}",
                snapshot
                    .run
                    .last_successful_discovery_at
                    .map_or("never".into(), local)
            );

            println!();
            println!(
                "next window {} .. {}",
                local(window.start),
                local(window.end)
            );

            println!();
            let live = snapshot.tracked.values().filter(|t| !t.terminal).count();
            println!("tracked     {}", snapshot.tracked.len());
            println!("  live      {live}");
            println!("  done      {}", snapshot.tracked.len() - live);

            println!();
            println!("github      {}", credentials(&config));
            Ok(0)
        }
        Command::Run {
            channels,
            dry_run,
            since_hours,
            json,
        } => {
            let dry_run = *dry_run || config.run.dry_run;
            let token = Secret::resolve(
                config.github.token_env.as_deref(),
                config.github.token_command.as_deref(),
            )
            .map_err(|e| e.to_string())?;

            let slack = slack_claude::from_config(&config, None).map_err(|e| e.to_string())?;
            let github = GitHubProvider::new(&config.github.api_url, token);
            let clock = SystemClock;

            let mut config = config;
            if !channels.is_empty() {
                config.slack.channels = channels.clone();
                config.slack.personal = false;
            }
            // Still needed with an explicit channel: only_my_messages filters
            // by author regardless of how a message was found.
            if config.slack.user_id.is_none() {
                config.slack.user_id = Some(resolve_identity(&slack, &store)?);
            }

            let run = Run {
                config: &config,
                source: &slack,
                provider: &github,
                sink: &slack,
                store: &store,
                clock: &clock,
                dry_run,
                since: since_hours.map(|h| Utc::now() - TimeDelta::hours(h)),
            };
            let report = run.execute().map_err(|e| e.to_string())?;
            report_out(&report, dry_run, *json, cli.common.verbose)?;
            Ok(report.exit_code())
        }
    }
}

fn logs(
    directory: &std::path::Path,
    lines: usize,
    grep: Option<&str>,
    follow: bool,
) -> Result<i32, String> {
    let recent = obs::tail(directory, lines, grep);
    if recent.is_empty() && !follow {
        // An empty directory and a filter that matched nothing look the same
        // in the output, so say which one happened.
        let hint = match grep {
            Some(needle) => format!("no log lines matching {needle:?}"),
            None => format!("no logs yet in {}", directory.display()),
        };
        println!("{hint}");
        return Ok(0);
    }
    for line in recent {
        println!("{line}");
    }
    if !follow {
        return Ok(0);
    }

    // Poll rather than watch: a log that appends a few lines per run doesn't
    // justify a filesystem-notification dependency. Tracking a byte offset in
    // the newest file keeps this to "read what arrived since last time".
    let mut current = obs::log_files(directory).pop();
    let mut offset = current
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map_or(0, |m| m.len() as usize);

    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));

        let newest = obs::log_files(directory).pop();
        if newest != current {
            // Rotated at midnight, or the first file appeared.
            current = newest;
            offset = 0;
        }
        let Some(path) = &current else { continue };
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        // Truncated out from under us; start over rather than slice past the end.
        if text.len() < offset {
            offset = 0;
        }
        for line in text[offset..].lines() {
            if grep.is_none_or(|n| line.to_lowercase().contains(&n.to_lowercase())) {
                println!("{line}");
            }
        }
        offset = text.len();
    }
}

fn init(path: &std::path::Path, force: bool) -> Result<i32, String> {
    if path.exists() && !force {
        return Err(format!(
            "{} already exists\n  pass --force to overwrite it",
            path.display()
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, STARTER_CONFIG).map_err(|e| e.to_string())?;

    println!("wrote {}", path.display());
    println!("\nnext:");
    println!("  1. edit it — at minimum confirm the emoji exist in your workspace");
    println!("  2. inkling config         show what it resolves to");
    println!("  3. inkling run --dry-run  see what it would do, change nothing");
    Ok(0)
}

/// Nobody knows their own Slack ID off-hand, so an omitted `slack.user_id` is
/// resolved from the credentials and cached — it costs a bridge call, and it
/// can't change for a given set of credentials.
fn resolve_identity<R: slack_claude::Runner>(
    slack: &slack_claude::SlackClaude<R>,
    store: &JsonStore,
) -> Result<String, String> {
    let mut snapshot = store.load().map_err(|e| e.to_string())?;
    if let Some(id) = &snapshot.user_id {
        return Ok(id.clone());
    }
    let me = slack.whoami().map_err(|e| e.to_string())?;
    snapshot.user_id = Some(me.id.clone());

    // whoami already carries the name, so seed it here rather than paying a
    // second profile call for the one author who appears on every message.
    let name = if me.name.trim().is_empty() {
        me.handle
    } else {
        me.name
    };
    if !name.is_empty() {
        snapshot.users.insert(me.id.clone(), name);
    }
    store.save(&snapshot).map_err(|e| e.to_string())?;
    Ok(me.id)
}

/// How many tracked records to list before summarizing the rest. Enough to
/// see everything on a normal week, short enough not to bury the summary.
const WATCH_LIMIT: usize = 15;

/// List what's being watched, so "tracking 7" says which seven.
///
/// One line per reference: state, what's on it, the ref, and its title —
/// `dpep/gqls#1` alone doesn't tell you what it is. Live records come first,
/// since finished ones are only waiting to age out.
fn watching(records: &[inkling::app::Watched]) {
    if records.is_empty() {
        return;
    }
    let (live, done): (Vec<_>, Vec<_>) = records.iter().partition(|r| !r.terminal);
    let ordered: Vec<_> = live.into_iter().chain(done).collect();

    let width = ordered
        .iter()
        .flat_map(|r| r.refs.iter().map(|(name, _)| name.len()))
        .max()
        .unwrap_or(0);

    println!("watching {}", ordered.len());
    for record in ordered.iter().take(WATCH_LIMIT) {
        let state = record.state.map_or("?", |s| s.as_str());
        let marks = match record.applied.is_empty() {
            true => "—".to_string(),
            false => record
                .applied
                .iter()
                .map(|e| format!(":{e}:"))
                .collect::<Vec<_>>()
                .join(" "),
        };
        for (name, title) in &record.refs {
            println!("  {state:<8}  {marks:<14}  {name:<width$}  {title}");
        }
    }
    if ordered.len() > WATCH_LIMIT {
        println!("  … and {} more", ordered.len() - WATCH_LIMIT);
    }
    println!();
}

/// Shorten a path under $HOME to `~/…`. These are printed to be read, and a
/// full home path is mostly a prefix you already know.
fn tildify(path: &std::path::Path) -> String {
    let text = path.display().to_string();
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => {
            let home = std::path::Path::new(&home).display().to_string();
            match text.strip_prefix(&home) {
                Some(rest) => format!("~{rest}"),
                None => text,
            }
        }
        _ => text,
    }
}

fn credentials(config: &Config) -> String {
    match Secret::resolve(
        config.github.token_env.as_deref(),
        config.github.token_command.as_deref(),
    ) {
        Ok(secret) if secret.is_empty() => "configured but empty".into(),
        Ok(_) => "ok".into(),
        Err(error) => format!("unavailable ({error})"),
    }
}

fn report_out(report: &Report, dry_run: bool, json: bool, verbose: u8) -> Result<(), String> {
    if json {
        // The detail rows carry everything; a summary object keeps the common
        // case readable.
        let out = serde_json::json!({
            "discovered": report.discovered,
            "tracked": report.tracked,
            "checked": report.checked,
            "added": report.added.iter().map(|(id, e)| {
                serde_json::json!({"message": id.to_string(), "emoji": e.to_string()})
            }).collect::<Vec<_>>(),
            "skipped": report.skipped,
            "failed": report.failed,
            "terminal": report.terminal,
            "dry_run": dry_run,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
        );
        return Ok(());
    }

    if dry_run {
        for detail in &report.details {
            let channel = if detail.channel_name.is_empty() {
                detail.id.channel.clone()
            } else {
                format!("#{}", detail.channel_name)
            };
            let posted = detail
                .posted_at
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M");
            let who = if detail.author.is_empty() {
                String::new()
            } else {
                format!(" by {}", detail.author)
            };
            println!("{channel}  {posted}{who}");
            println!("  link     {}", detail.permalink);

            for (reference, author) in &detail.references {
                // The person who posted the link often isn't the person who
                // wrote the code; saying so only when they differ keeps the
                // common case quiet.
                let author = match author.as_str() {
                    "" => String::new(),
                    a if a.eq_ignore_ascii_case(&detail.author) => String::new(),
                    a => format!(" (opened by {a})"),
                };
                println!("  ref      {reference}{author}");
            }
            let states: Vec<String> = detail.states.iter().map(ToString::to_string).collect();
            println!("  state    {}", states.join(", "));
            println!(
                "  existing {}",
                match &detail.existing {
                    Some(names) if names.is_empty() => "none".to_string(),
                    Some(names) => names
                        .iter()
                        .map(|n| format!(":{n}:"))
                        .collect::<Vec<_>>()
                        .join(" "),
                    None => "unknown".to_string(),
                }
            );
            println!(
                "  action   {}",
                match (&detail.plan.add, &detail.plan.satisfied_by) {
                    (Some(emoji), _) => format!("add :{emoji}:"),
                    // Usually an alias, so naming the canonical here would be
                    // a claim about a reaction that isn't on the message.
                    (None, Some(found)) => format!("nothing (already signaled by :{found}:)"),
                    (None, None) => "nothing (no signal for this state)".to_string(),
                }
            );
            println!();
        }
    }

    // A discovery failure isn't a per-record failure, so the counters below
    // all read zero while the run exits nonzero. Say it outright.
    if report.discovery_failed() {
        println!("discovery FAILED — the search window was not covered; see the log");
    }

    if verbose > 0 {
        watching(&report.watching);
    }

    let verb = if dry_run { "would add" } else { "added" };
    println!(
        "discovered {} · checked {} · {verb} {} · skipped {} · failed {} · tracking {} ({} done)",
        report.discovered,
        report.checked,
        report.added.len(),
        report.skipped,
        report.failed,
        report.tracked,
        report.terminal,
    );
    Ok(())
}
