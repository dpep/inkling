//! `inkling` — annotate Slack messages with the state of what they link to.

use chrono::{TimeDelta, Utc};
use clap::{Args, Parser, Subcommand};
use inkling::adapters::github::GitHubProvider;
use inkling::adapters::slack_claude::{ClaudeRunner, SlackClaude};
use inkling::adapters::store_json::{JsonStore, Store};
use inkling::app::{Report, Run};
use inkling::config::Config;
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
    /// Where daily log files go. Defaults beside the state file; "off"
    /// disables file logging.
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
    /// Validate the config and print what it resolves to.
    Check,
}

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

    match run(&cli) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("inkling: {error}");
            std::process::exit(2);
        }
    }
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
    let config = Config::load(&path).map_err(|e| e.to_string())?;
    let store = JsonStore::new(
        cli.common
            .state
            .clone()
            .unwrap_or_else(JsonStore::default_path),
    );

    match &cli.command {
        Command::Check => {
            println!("config    {}", path.display());
            println!("state     {}", store.path().display());
            println!("personal  {}", config.slack.personal);
            println!("channels  {}", config.slack.channels.join(", "));
            println!("mine only {}", config.filters.only_my_messages);
            for state in inkling::domain::model::State::ALL {
                if let Some(emoji) = config.canonical_for(state) {
                    println!("{state:9} :{emoji}:");
                }
            }
            let token = credentials(&config);
            println!("github    {token}");
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
            println!("last run    {:?}", snapshot.run.last_run_outcome);
            println!(
                "last search {}",
                snapshot
                    .run
                    .last_successful_discovery_at
                    .map_or("never".into(), |t| t.to_rfc3339())
            );
            println!(
                "next window {} .. {}",
                window.start.to_rfc3339(),
                window.end.to_rfc3339()
            );
            println!("tracked     {}", snapshot.tracked.len());
            println!(
                "  live      {}",
                snapshot.tracked.values().filter(|t| !t.terminal).count()
            );
            println!("github      {}", credentials(&config));
            Ok(0)
        }
        Command::Run {
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

            let slack = SlackClaude::new(ClaudeRunner::default());
            let github = GitHubProvider::new(&config.github.api_url, token);
            let clock = SystemClock;

            let mut config = config;
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
            report_out(&report, dry_run, *json)?;
            Ok(report.exit_code())
        }
    }
}

/// Nobody knows their own Slack ID off-hand, so an omitted `slack.user_id` is
/// resolved from the credentials and cached — it costs a bridge call, and it
/// can't change for a given set of credentials.
fn resolve_identity<R: inkling::adapters::slack_claude::Runner>(
    slack: &SlackClaude<R>,
    store: &JsonStore,
) -> Result<String, String> {
    let mut snapshot = store.load().map_err(|e| e.to_string())?;
    if let Some(id) = &snapshot.user_id {
        return Ok(id.clone());
    }
    let me = slack.whoami().map_err(|e| e.to_string())?;
    snapshot.user_id = Some(me.id.clone());
    store.save(&snapshot).map_err(|e| e.to_string())?;
    Ok(me.id)
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

fn report_out(report: &Report, dry_run: bool, json: bool) -> Result<(), String> {
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
            println!("{}", detail.permalink);
            println!("  refs     {}", detail.references.join(", "));
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
                match &detail.plan.add {
                    Some(emoji) => format!("add :{emoji}:"),
                    None => match detail.plan.desired.as_ref() {
                        Some(emoji) => format!("nothing (:{emoji}: already present)"),
                        None => "nothing (no signal for this state)".to_string(),
                    },
                }
            );
            println!();
        }
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
