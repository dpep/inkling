//! `inkling` — annotate Slack messages with the state of what they link to.

use chrono::{DateTime, SubsecRound, TimeDelta, Utc};
use clap::{Args, Parser, Subcommand};
use inkling::adapters::github::GitHubProvider;
use inkling::adapters::slack_claude;
use inkling::adapters::store_json::{JsonStore, Store};
use inkling::adapters::watches_json::Unwatched;
use inkling::app::{Report, Run};
use inkling::config::Config;
use inkling::config_edit;
use inkling::obs;
use inkling::ports::{Clock, SystemClock};
use inkling::secret::Secret;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "inkling",
    version,
    about = "Tell an agent when a pull request or Jira issue it cares about changes.",
    long_about = "Watches GitHub pull requests and Jira issues and reports each transition \
once: checks going red or green, an approval, a merge, a status change.\n\n\
`wait` blocks until a pull request gets somewhere; `watch` and `events` register \
interest and read what moved later; `subscribe` and `hook` push news into a \
Claude Code session as it happens.\n\n\
Start with `inkling wait <pr>`, or install the hooks from the README."
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
    /// Where waiters coordinate. Defaults to $XDG_STATE_HOME/inkling/waits.json
    #[arg(long, global = true)]
    waits: Option<PathBuf>,
    /// Watches and their events. Defaults to $XDG_STATE_HOME/inkling/watches.json
    #[arg(long, global = true)]
    watches: Option<PathBuf>,
    /// More detail; repeat for more (-vv).
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
    // Hidden: the Slack reactions are parked, not removed (docs/deferred.md).
    #[command(hide = true)]
    Run {
        /// Channels to search, or pull requests to check now.
        ///
        /// A channel handle or ID narrows the run to it, turning off personal
        /// mode. A pull request — a URL, `owner/repo#12`, or a shorter form
        /// matching something tracked — is checked immediately, ignoring the
        /// backoff schedule.
        targets: Vec<String>,
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
    /// Block until pull requests move.
    ///
    /// By default that means any change at all — the next step, whatever it
    /// turns out to be. Name a condition with --until to wait for something
    /// specific.
    ///
    /// Exits 0 when the condition is met, 3 when it can no longer be met
    /// (closed while waiting for a merge, a red suite, or nothing left to
    /// change), 4 on timeout, and 5 when a reference can't be read. Composes
    /// with the shell: `inkling wait --until merged 42 && deploy`.
    Wait {
        /// Pull requests to wait on — a URL, `owner/repo#12`, or a shorter
        /// form matching something already watched. Several means all of them
        /// must satisfy the condition.
        targets: Vec<String>,
        /// What to wait for. The default, `changed`, returns on the next
        /// move in any direction. `done` is either terminal state, merged or
        /// closed; `checks` is CI finishing, green or red.
        #[arg(long, short, default_value = "changed", value_parser = parse_until)]
        until: inkling::domain::wait::Until,
        /// Give up after this long, e.g. 90s, 30m, 2h.
        #[arg(long, short, default_value = "30m", value_parser = humantime::parse_duration)]
        timeout: std::time::Duration,
        /// How often to look.
        #[arg(long, default_value = "15s", value_parser = humantime::parse_duration)]
        interval: std::time::Duration,
        /// Emit the outcome as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Watch things, and learn later what changed.
    ///
    /// Registers interest and returns. Nothing blocks and nothing runs in the
    /// background: `inkling events` reads what has moved since it last
    /// looked, polling first if its readings have gone stale.
    ///
    /// With no arguments, lists what is being watched. Every watch expires at
    /// its `--ttl`, and that expiry is the only cleanup there is.
    Watch {
        /// Pull requests — a URL, `owner/repo#12`, or a shorter form matching
        /// something watched — and Jira issues, as a key like `AI-123` or a
        /// browse URL.
        targets: Vec<String>,
        /// Stop watching after this long, e.g. 2h, 24h, 7d.
        // Must equal watches_json::WATCH_TTL; a test holds them together.
        #[arg(long, default_value = "72h", value_parser = humantime::parse_duration)]
        ttl: std::time::Duration,
        /// Emit the watch list as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Stop watching.
    Unwatch {
        /// What to stop watching, in any form `watch` accepts.
        targets: Vec<String>,
        /// Stop watching everything.
        #[arg(long)]
        all: bool,
    },
    /// Print what has changed since last time, and nothing when nothing has.
    ///
    /// Each event is a transition — which reference, from what to what, and
    /// when — never a snapshot.
    ///
    /// Every reader has its own stream: you are told about the things you are
    /// watching, and nothing else. `--subscriber <id>` keeps your place here,
    /// for a caller with nowhere to put one; `--since <cursor>` resumes from
    /// a cursor you kept, which `--json` prints on every row. With neither,
    /// the reader is the session you are in, or the terminal you are at.
    ///
    /// Exits 0 whether or not anything had changed.
    Events {
        /// Resume from a cursor a previous run printed, e.g. `cli:7`.
        #[arg(long, conflicts_with = "subscriber", value_parser = parse_cursor)]
        since: Option<inkling::domain::watch::Cursor>,
        /// A named cursor Inkling keeps, e.g. a session id.
        #[arg(long)]
        subscriber: Option<String>,
        /// Look upstream first for watches that are due a reading, but never
        /// for one younger than this. A watch is due at 1/24 of how long it
        /// has been quiet, between 1m and 60m. Fresh readings cost nothing
        /// but a local file read.
        #[arg(long, default_value = "60s", value_parser = humantime::parse_duration)]
        refresh: std::time::Duration,
        /// Emit the events as a JSON array.
        #[arg(long)]
        json: bool,
        /// Emit one JSON object per line.
        #[arg(long, conflicts_with = "json")]
        ndjson: bool,
    },
    /// Have this session told the moment something moves.
    ///
    /// Attaches the session's inbox — Claude Code exports the socket and
    /// token to its hooks and Bash commands — so a resident inkling can push
    /// a transition in rather than wait to be asked. Attach and detach, not
    /// create and delete: your cursor and your watches live on either way, so
    /// `claude --resume` picks up the net change since you left.
    Subscribe {
        /// The session id to attach to. Defaults to the one Claude Code
        /// exports.
        #[arg(long)]
        session: Option<String>,
    },
    /// Stop pushing to this session. Its watches and its place in the stream
    /// stay until their own expiry, so nothing is lost by ending a session.
    Unsubscribe {
        #[arg(long)]
        session: Option<String>,
    },
    /// Claude Code's side of the wiring: reads a hook payload on stdin.
    ///
    /// Three one-line commands in settings.json instead of shell that picks
    /// the session id out of JSON. A hook never fails a session — whatever
    /// goes wrong, this logs it and exits 0 saying nothing.
    Hook {
        /// Which hook is calling.
        #[arg(value_enum)]
        event: HookEvent,
    },
    /// The resident server. Started when it is needed and gone when it
    /// isn't; nothing runs this by hand.
    #[command(hide = true)]
    Serve {
        /// Wake the running server, if any, and exit. For `make install`: a
        /// shell can't write to a FIFO without blocking when nobody reads it.
        #[arg(long)]
        ring: bool,
    },
    /// Show watches, the resident server, subscribers, and credential status.
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
        /// Only lines containing this text, case-insensitively. A substring,
        /// not a regular expression — alternation and character classes are
        /// matched literally.
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

/// The hooks inkling answers, named as Claude Code names them.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum HookEvent {
    /// SessionStart — attach this session's inbox.
    SessionStart,
    /// SessionEnd — detach it.
    SessionEnd,
    /// UserPromptSubmit — print anything the push didn't deliver.
    Prompt,
    /// PostToolUse — watch what the session just touched.
    PostTool,
}

impl HookEvent {
    fn as_str(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "session-start",
            HookEvent::SessionEnd => "session-end",
            HookEvent::Prompt => "prompt",
            HookEvent::PostTool => "post-tool",
        }
    }
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
        /// Dotted path, e.g. github.token_command or serve.idle_after
        key: String,
    },
    /// Set one key. An empty value removes it.
    Set {
        /// Dotted path, e.g. github.token_command or serve.idle_after
        key: String,
        /// Type is inferred: true/false, whole numbers, comma-separated
        /// lists, otherwise a string. Pass "" to remove the key.
        value: String,
    },
}

/// What `inkling config init` writes. Every value is a default, so the file
/// changes nothing until edited; it exists to show where the knobs are.
const STARTER_CONFIG: &str = r##"# Inkling — tells an agent when something it cares about changes.
# Every field here has a default; this file is a starting point, not a schema.

[github]
token_command = "gh auth token"   # or token_env = "GITHUB_TOKEN"
# Exact check names that never turn a pull request red or hold it pending.
# ignore_checks = ["merge-lock"]

# The resident server, started by `subscribe` or `watch` when needed.
[serve]
# interval = "15s"      # the shortest nap; otherwise it sleeps until a watch is due
# idle_after = "5m"     # how long with nothing to do before it stands down
# max_lifetime = "24h"  # so a rebuilt binary eventually gets picked up
"##;

fn main() {
    // Rust ignores SIGPIPE, so writing to a closed pipe returns EPIPE and
    // println! panics — `inkling logs | head` would abort with a backtrace
    // instead of stopping. Restore the default so it exits quietly, like
    // every other command in a pipeline.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };

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
            // Both sinks, deliberately. A server spawned by a hook has its
            // stdio on /dev/null and no terminal to print to, so until this
            // line existed a `serve` that died on its first act — an
            // unresolvable token, most often — died without saying anything
            // anywhere, while every hook went on spawning another one.
            tracing::error!(error, "inkling failed");
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

/// Where the log files are, for reading them back. `off` suppresses writing
/// them, not reading whatever is already there.
fn log_directory(common: &Common) -> PathBuf {
    match common.log_dir.as_deref() {
        Some("off") | None => obs::default_log_directory(),
        Some(dir) => PathBuf::from(dir),
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
            let directory = log_directory(&cli.common);
            if *show_path {
                println!("{}", directory.display());
                return Ok(0);
            }
            return logs(&directory, *lines, grep.as_deref(), *follow);
        }
        // A broken config mustn't fail the install that rings.
        Command::Serve { ring: true } => {
            inkling::serve::ring(&inkling::serve::Config::in_dir(serve_dir(&cli.common)).doorbell);
            return Ok(0);
        }
        // Before the config load, so a config that won't parse is this hook's
        // failure rather than the session's — see `swallowing`.
        Command::Hook { event } => {
            if inside_bridge(std::env::var_os(slack_claude::BRIDGE_ENV)) {
                return Ok(0);
            }
            return Ok(swallowing(*event, || {
                let config = Config::load(&path).map_err(|e| e.to_string())?;
                hook(*event, &config, &cli.common)
            }));
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
        Command::Subscribe { session } => {
            let id = session_id(session.as_deref())?;
            let replaced = subscribe(&id, &config, &cli.common)?;
            println!(
                "{} {}",
                match replaced {
                    true => "re-attached",
                    false => "subscribed",
                },
                short(&id)
            );
            Ok(0)
        }
        Command::Unsubscribe { session } => {
            let id = session_id(session.as_deref())?;
            match unsubscribe(&id, &cli.common)? {
                true => println!("unsubscribed {}", short(&id)),
                false => println!("{}: not subscribed", short(&id)),
            }
            Ok(0)
        }
        // Handled above, before the config load they must not depend on.
        Command::Hook { .. } | Command::Serve { ring: true } => unreachable!(),
        Command::Serve { ring: false } => {
            let token = Secret::resolve(
                config.github.token_env.as_deref(),
                config.github.token_command.as_deref(),
            )
            .map_err(|e| e.to_string())?;

            let watches = watch_store(&cli.common);
            let github = GitHubProvider::from_config(&config.github, token);
            let jira = inkling::adapters::jira_acli::JiraProvider::new();
            let slack = slack_threads(&config, store.path())?;
            let clock = SystemClock;
            let work = inkling::push::Tick {
                poller: inkling::watch::Poller {
                    store: &watches,
                    github: &github,
                    jira: &jira,
                    slack: Some(&slack),
                    clock: &clock,
                },
                sessions: &inkling::push::RealSessions,
                clock: &clock,
            };

            match inkling::serve::run(serve_config(&config, &cli.common), &work)
                .map_err(|e| e.to_string())?
            {
                inkling::serve::Outcome::AlreadyRunning => Ok(0),
                inkling::serve::Outcome::Served(_) => Ok(0),
            }
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
                    .format("%Y-%m-%d %H:%M %Z")
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

            let watched = watch_store(&cli.common).read();
            let live = watched
                .watches
                .values()
                .filter(|w| w.deadline > now)
                .count();
            println!();
            println!("watching    {live}");
            println!("  events    {} in the last day", watched.events.len());

            println!();
            serve_out(&inkling::serve::read_status(&serve_config(
                &config,
                &cli.common,
            )));

            println!();
            subscribers_out(&watched, now);

            println!();
            println!("github      {}", credentials(&config));

            println!();
            last_day_out(&log_directory(&cli.common), now);
            Ok(0)
        }
        Command::Watch { targets, ttl, json } => {
            let watches = watch_store(&cli.common);
            let clock = SystemClock;
            let mut refused = Vec::new();
            let owner = reader(None, TERMINAL, &watches.read());

            if !targets.is_empty() {
                let snapshot = store.load().map_err(|e| e.to_string())?;
                let known = known(&snapshot, &watches.read());
                let references;
                (references, refused) = watch_targets(targets, &config.github.hosts, &known);
                for problem in &refused {
                    println!("{problem}");
                }
                // Whole seconds, where the value is built: a deadline a day
                // out has no microseconds' worth of meaning, and printing
                // them anywhere downstream would invent some.
                let deadline = (clock.now()
                    + TimeDelta::from_std(*ttl).map_err(|_| "--ttl is too long".to_string())?)
                .trunc_subsecs(0);

                watches
                    .update(clock.now(), |file| {
                        for reference in &references {
                            file.watch(reference, &owner, deadline, clock.now());
                            tracing::info!(
                                %reference,
                                owner,
                                deadline = stamp(deadline),
                                "watch registered"
                            );
                        }
                    })
                    .map_err(|e| e.to_string())?;

                // Read them now rather than leaving the baseline to whenever
                // `events` next runs: a transition in that gap would be
                // swallowed into the baseline, which is the one thing the
                // caller just asked not to happen.
                if !references.is_empty() {
                    let slack = slack_threads(&config, store.path())?;
                    match poll(&config, &watches, &clock, None, Some(&slack), "watch") {
                        Ok(polled) => {
                            for line in finished_already(&references, &polled, &watches.read()) {
                                println!("{line}");
                            }
                        }
                        Err(error) => {
                            println!("could not read them just now: {error}");
                            println!("  the next `inkling events` will pick them up");
                        }
                    }
                }
            }

            // Here as well as in `subscribe`, because the two halves of a
            // reason to serve arrive in either order: a session subscribes
            // before it has watched anything, and a watch outlives the server
            // that stood down when the last one finished.
            ensure_serving(&config, &cli.common, "watch");

            watch_out(&watches.read(), &owner, clock.now(), *json)?;
            // The good references are watched either way; the exit code is
            // where a script learns that one of its arguments was not.
            Ok(match refused.is_empty() {
                true => 0,
                false => 1,
            })
        }
        Command::Unwatch { targets, all } => {
            if targets.is_empty() && !all {
                return Err("unwatch needs something to stop watching, or --all".into());
            }
            let watches = watch_store(&cli.common);
            let clock = SystemClock;
            let owner = reader(None, TERMINAL, &watches.read());

            if *all {
                let dropped = watches
                    .update(clock.now(), |file| file.unwatch_all(&owner))
                    .map_err(|e| e.to_string())?;
                println!("stopped watching {dropped}");
                return Ok(0);
            }

            let snapshot = store.load().map_err(|e| e.to_string())?;
            let known = known(&snapshot, &watches.read());
            let (references, refused) = watch_targets(targets, &config.github.hosts, &known);
            for problem in &refused {
                println!("{problem}");
            }
            let removed = watches
                .update(clock.now(), |file| {
                    references
                        .iter()
                        .map(|reference| (reference.clone(), file.unwatch(reference, &owner)))
                        .collect::<Vec<_>>()
                })
                .map_err(|e| e.to_string())?;

            for (reference, outcome) in removed {
                match outcome {
                    Unwatched::Removed => {
                        tracing::info!(%reference, owner, "watch removed");
                        println!("stopped watching {reference}");
                    }
                    // Somebody else asked for it too, and one poll serves
                    // both — so saying "stopped watching" without the caveat
                    // would misdescribe what is still on the list.
                    Unwatched::StillWatched => {
                        tracing::info!(%reference, owner, "watch released");
                        println!("stopped watching {reference} (another reader still is)");
                    }
                    Unwatched::Absent => println!("{reference}: you aren't watching that"),
                }
            }
            Ok(match refused.is_empty() {
                true => 0,
                false => 1,
            })
        }
        Command::Events {
            since,
            subscriber,
            refresh,
            json,
            ndjson,
        } => {
            let watches = watch_store(&cli.common);
            let clock = SystemClock;

            // Only when the readings have aged: resolving the GitHub token
            // shells out, and this runs on every prompt.
            let refresh = TimeDelta::from_std(*refresh).unwrap_or(TimeDelta::zero());
            if watches.read().due(clock.now(), refresh) {
                // A provider that can't be reached is a log line, not a
                // failure — a blip must not cost an agent its turn.
                let slack = slack_threads(&config, store.path())?;
                if let Err(error) = poll(
                    &config,
                    &watches,
                    &clock,
                    Some(refresh),
                    Some(&slack),
                    "events",
                ) {
                    tracing::warn!(%error, "could not refresh watches");
                }
            }

            // Two ways to name a reader and a place in its stream: a cursor
            // the caller kept, or one inkling keeps for it. Neither is a way
            // to read somebody else's.
            let (owner, events) = match since {
                Some(cursor) => {
                    let file = watches.read();
                    if !file.knows(&cursor.owner) {
                        return Err(format!(
                            "{}: no such reader — its cursor was collected after \
                             {} days of silence, so start again with --subscriber",
                            cursor.owner,
                            inkling::adapters::watches_json::SUBSCRIBER_RETENTION.num_days(),
                        ));
                    }
                    let due =
                        inkling::domain::watch::addressed(&file.events, cursor.seq, &cursor.owner)
                            .into_iter()
                            .cloned()
                            .collect();
                    (cursor.owner.clone(), due)
                }
                None => {
                    let owner = reader(subscriber.as_deref(), TERMINAL, &watches.read());
                    let due = watches
                        .update(clock.now(), |file| file.take(&owner, clock.now()))
                        .map_err(|e| e.to_string())?;
                    (owner, due)
                }
            };

            events_out(&owner, &events, *json, *ndjson)?;
            Ok(0)
        }
        Command::Wait {
            targets,
            until,
            timeout,
            interval,
            json,
        } => {
            if targets.is_empty() {
                return Err("wait needs at least one pull request".into());
            }
            let token = Secret::resolve(
                config.github.token_env.as_deref(),
                config.github.token_command.as_deref(),
            )
            .map_err(|e| e.to_string())?;

            // Read-only, and no lock: a wait never writes either file. It
            // reads them only to expand short references like `inkling#2`.
            let snapshot = store.load().map_err(|e| e.to_string())?;
            let known = known(&snapshot, &watch_store(&cli.common).read());
            let references = wait_targets(targets, &config.github.hosts, &known)?;

            let github = GitHubProvider::from_config(&config.github, token);
            let clock = SystemClock;

            // Waiters coordinate through a file so N of them cost one poll.
            // With a single waiter this is the same query plus a lock and a
            // small write — the shared path is not worth a flag to avoid.
            let waits_path = cli
                .common
                .waits
                .clone()
                .unwrap_or_else(inkling::adapters::waits_json::Waits::default_path);
            let shared = inkling::adapters::waits_json::SharedObserver::new(
                waits_path,
                clock.now() + TimeDelta::from_std(*timeout).unwrap_or(TimeDelta::zero()),
                *interval,
                &github,
                &clock,
            );

            let watching = references.len();
            let report = inkling::wait::Wait {
                references,
                until: *until,
                timeout: *timeout,
                interval: *interval,
                settle: inkling::wait::SETTLE,
                observer: &shared,
                clock: &clock,
                sleeper: &inkling::ports::RealSleeper,
            }
            .execute()
            .map_err(|e| e.to_string())?;

            // One line per wait, to the log file rather than stdout. The
            // question docs/waiting.md leaves open — whether sharing a poller
            // ever carries load — is answered by `shared` and `peak_watches`
            // across many of these, not by any single run.
            let shared = shared.stats();
            tracing::info!(
                until = %report.until,
                outcome = report.outcome.as_str(),
                waited_seconds = report.waited.as_secs(),
                polls = report.polls,
                watching,
                polled = shared.polled,
                shared = shared.shared,
                starved = shared.starved,
                peak_watches = shared.peak_watches,
                peak_batch = shared.peak_batch,
                takeovers = shared.takeovers,
                incomplete = report.incomplete,
                failures = report.failures,
                "wait finished"
            );

            wait_out(&report, *json)?;
            Ok(report.exit_code())
        }
        Command::Run {
            targets,
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
            let github = GitHubProvider::from_config(&config.github, token);
            let clock = SystemClock;

            // One writer at a time. The snapshot is rewritten whole, so two
            // overlapping runs would silently last-writer-win — and the
            // casualty would be the ownership ledger, which cannot be
            // reconstructed from anything else.
            let lock_path = inkling::adapters::lock::FileLock::path_for(store.path());
            let _lock = match inkling::adapters::lock::FileLock::try_acquire(&lock_path) {
                Ok(Some(lock)) => lock,
                Ok(None) => {
                    // Cron overlap is expected, not a fault: the next run
                    // picks up whatever this one leaves.
                    tracing::info!("another run holds the state lock; leaving it to finish");
                    return Ok(0);
                }
                Err(e) => return Err(e.to_string()),
            };

            let mut config = config;
            let snapshot = store.load().map_err(|e| e.to_string())?;
            let known = known(&snapshot, &watch_store(&cli.common).read());
            let (channels, force) = classify(targets, &config.github.hosts, &known)?;
            if !channels.is_empty() {
                config.slack.channels = channels;
                config.slack.personal = false;
            } else if !force.is_empty() {
                // Only pull requests named: "check this one now" needs no
                // search, since the message is already tracked.
                config.slack.channels.clear();
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
                force,
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
    println!("  1. edit it, if a default doesn't suit");
    println!("  2. inkling config         show what it resolves to");
    println!("  3. inkling watch <pr>     start watching something");
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
const TRACKING_LIMIT: usize = 15;

/// List the tracked records, so "tracking 7" says which seven.
///
/// One line per reference: state, what's on it, the ref, and its title —
/// `dpep/gqls#1` alone doesn't tell you what it is. Live records come first,
/// since finished ones are only waiting to age out.
fn tracking(records: &[inkling::app::Tracked]) {
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

    // Titles are arbitrary prose. Left whole they wrap, and a wrapped row
    // stops being a row — the columns no longer line up with anything.
    let used = 2 + 8 + 2 + 14 + 2 + width + 2;
    let budget = terminal_width().saturating_sub(used).max(24);

    println!("tracking");
    for record in ordered.iter().take(TRACKING_LIMIT) {
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
            let title = ellipsize(title, budget);
            println!("  {state:<8}  {marks:<14}  {name:<width$}  {title}");
        }
    }
    if ordered.len() > TRACKING_LIMIT {
        println!("  … and {} more", ordered.len() - TRACKING_LIMIT);
    }
    println!();
}

/// Everything a short reference can select among: what is watched, plus what
/// the parked Slack reactions track. Watches alone would leave `run`'s records
/// unreachable; tracked alone made `wait 42` fail for anything not seen in Slack.
fn known(
    snapshot: &inkling::adapters::store_json::Snapshot,
    watches: &inkling::adapters::watches_json::Watches,
) -> Vec<inkling::domain::model::Reference> {
    let tracked = snapshot.tracked.values().flat_map(|t| t.references.iter());
    let watched = watches.watches.values().map(|w| &w.reference);
    tracked.chain(watched).cloned().collect()
}

/// Fill in the parts of a reference the caller left out.
///
/// The one rule, for every command that takes a reference: `owner/repo#N`
/// names a pull request completely once the host defaults, and only the forms
/// shorter than that — `repo#N`, `#N`, `N` — need a tracked record, where a
/// short form genuinely selects among what is tracked.
///
/// A rule per command is what dogfooding caught: `watch` learned the first
/// half and `wait` did not, and both suites stayed green.
fn complete(
    owner: Option<String>,
    repo: Option<String>,
    number: u64,
    hosts: &[String],
    known: &[inkling::domain::model::Reference],
) -> Result<inkling::domain::model::Reference, String> {
    match (owner, repo) {
        (Some(owner), Some(repo)) => match hosts.first() {
            Some(host) => Ok(inkling::domain::model::Reference::GitHubPullRequest {
                host: host.clone(),
                owner,
                repo,
                number,
            }),
            None => Err(format!(
                "{owner}/{repo}#{number}: no github host is configured — give the full reference"
            )),
        },
        (owner, repo) => resolve(&owner, &repo, number, known),
    }
}

/// Find the one tracked record a short reference selects. Exactly one match or
/// nothing: reacting to the wrong pull request is worse than being asked for a
/// fuller reference.
fn resolve(
    owner: &Option<String>,
    repo: &Option<String>,
    number: u64,
    known: &[inkling::domain::model::Reference],
) -> Result<inkling::domain::model::Reference, String> {
    use inkling::domain::model::Reference::GitHubPullRequest;

    let mut matches: Vec<_> = known
        .iter()
        .filter(|reference| {
            // A partial reference is always a pull request; a Jira key can
            // only be given in full, so there is nothing to fill in.
            let GitHubPullRequest {
                owner: o,
                repo: r,
                number: n,
                ..
            } = reference
            else {
                return false;
            };
            *n == number
                && repo
                    .as_ref()
                    .is_none_or(|want| want.eq_ignore_ascii_case(r))
                && owner
                    .as_ref()
                    .is_none_or(|want| want.eq_ignore_ascii_case(o))
        })
        .cloned()
        .collect();
    matches.sort();
    matches.dedup();

    let given = match (owner, repo) {
        (Some(o), Some(r)) => format!("{o}/{r}#{number}"),
        (_, Some(r)) => format!("{r}#{number}"),
        _ => format!("#{number}"),
    };
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(format!(
            "{given}: nothing watched matches it — give the full reference, \
             e.g. github.com/owner/repo#{number}"
        )),
        _ => {
            let names: Vec<String> = matches.iter().map(ToString::to_string).collect();
            Err(format!("{given} is ambiguous: {}", names.join(", ")))
        }
    }
}

/// Split `run`'s arguments into channels to search and pull requests to
/// check now.
///
/// A bare number is resolved against what's already tracked, and only when
/// exactly one record matches. Guessing a repository would react to the
/// wrong pull request, which is worse than asking for a fuller reference.
/// clap needs an owned error; `Until`'s own is already one.
fn parse_until(arg: &str) -> Result<inkling::domain::wait::Until, String> {
    arg.parse()
        .map_err(|e: inkling::domain::wait::UnknownUntil| e.to_string())
}

/// The same targets `run` takes, minus channels — there is no such thing as
/// waiting for a channel, and reading one as a typo'd pull request is kinder
/// than polling something that will never resolve.
fn wait_targets(
    targets: &[String],
    hosts: &[String],
    known: &[inkling::domain::model::Reference],
) -> Result<Vec<inkling::domain::model::Reference>, String> {
    let (channels, references) = classify(targets, hosts, known)?;
    if let Some(channel) = channels.first() {
        return Err(format!("{channel}: wait takes pull requests, not channels"));
    }
    // A Jira issue has no merge ladder to threshold against, so `--until`
    // could say nothing about one. Watching it is the answer that exists.
    if let Some(message) = references
        .iter()
        .find(|r| matches!(r, inkling::domain::model::Reference::SlackMessage { .. }))
    {
        return Err(format!(
            "{message}: wait takes pull requests — for a Slack message use \
             `inkling watch` and `inkling events`"
        ));
    }
    if let Some(issue) = references
        .iter()
        .find(|r| matches!(r, inkling::domain::model::Reference::JiraIssue { .. }))
    {
        return Err(format!(
            "{issue}: wait takes pull requests — for a Jira issue use \
             `inkling watch {issue}` and `inkling events`"
        ));
    }
    Ok(references)
}

fn classify(
    targets: &[String],
    hosts: &[String],
    known: &[inkling::domain::model::Reference],
) -> Result<(Vec<String>, Vec<inkling::domain::model::Reference>), String> {
    use inkling::domain::detect::{Target, parse_target};

    let mut channels = Vec::new();
    let mut force = Vec::new();

    for target in targets {
        match parse_target(target, hosts)? {
            Target::Channel(channel) => channels.push(channel),
            Target::Reference(reference) => force.push(reference),
            Target::Partial {
                owner,
                repo,
                number,
            } => force.push(complete(owner, repo, number, hosts, known)?),
        }
    }
    Ok((channels, force))
}

/// What Claude Code exports to a session's hooks and its Bash commands.
/// Verified on 2.1.278; the messaging pair appears only in the CLI's debug
/// log, so treat both as unstable.
const SOCKET_ENV: &str = "CLAUDE_CODE_MESSAGING_SOCKET";
const SESSION_ENV: &str = "CLAUDE_CODE_SESSION_ID";

/// The reader a person at a terminal is. Every watch has an owner and every
/// read names one, so there is one rule rather than an owned path and an
/// unowned one.
const TERMINAL: &str = "cli";

/// Which reader this invocation is.
///
/// A session's Bash carries the messaging socket in its environment, and the
/// subscriber attached to that socket is the session that will actually be
/// pushed to — which is the identity that matters, rather than whatever id
/// the calling process holds. (A subagent's Bash has its own session id and
/// its parent's socket.) Failing that, the session id Claude Code exports.
/// Failing that, the person at the terminal.
fn reader(
    explicit: Option<&str>,
    fallback: &str,
    watches: &inkling::adapters::watches_json::Watches,
) -> String {
    if let Some(id) = explicit {
        return id.to_string();
    }
    if let Some(socket) = std::env::var_os(SOCKET_ENV)
        && let Some(subscriber) = watches.subscriber_at(std::path::Path::new(&socket))
    {
        return subscriber.to_string();
    }
    std::env::var(SESSION_ENV)
        .ok()
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

const TOKEN_ENV: &str = "CLAUDE_CODE_MESSAGING_TOKEN";

/// Which session this is — for attaching and detaching, where the answer has
/// to be the session itself rather than whichever reader happens to be
/// nearest.
fn session_id(explicit: Option<&str>) -> Result<String, String> {
    explicit
        .map(str::to_string)
        .or_else(|| std::env::var(SESSION_ENV).ok())
        .filter(|id| !id.is_empty())
        .ok_or_else(|| format!("no session id — pass --session, or run where {SESSION_ENV} is set"))
}

/// Enough of a session id to tell two apart on a line, which is all a status
/// table needs. Never a token.
fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Where the server keeps its lock and its status file.
///
/// Beside the watch store rather than at a fixed path, so pointing
/// `--watches` at a scratch directory moves the whole server with it instead
/// of leaving a test talking to the real one's lock.
fn serve_config(config: &Config, common: &Common) -> inkling::serve::Config {
    inkling::serve::Config {
        interval: config.serve.interval,
        idle_after: config.serve.idle_after,
        max_lifetime: config.serve.max_lifetime,
        ..inkling::serve::Config::in_dir(serve_dir(common))
    }
}

fn serve_dir(common: &Common) -> PathBuf {
    watch_store(common)
        .path()
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(inkling::serve::Config::default_dir)
}

/// Wake the running server, or start one if there is now a reason for one.
///
/// The reason is the same predicate a running server uses to decide whether
/// to stay, so one is never spawned only to stand straight back down, and
/// never missing while there is something to deliver. Failing to start one
/// is a log line: the pull feed still works.
fn ensure_serving(config: &Config, common: &Common, why: &str) {
    // First, and whether or not this caller has a reason to start one: the
    // watch list just changed, and a running server may be asleep until an
    // hour from now on a plan that predates it.
    inkling::serve::ring(&serve_config(config, common).doorbell);
    let store = watch_store(common);
    if !inkling::push::should_serve(&store.read(), Utc::now()) {
        return;
    }
    // The child re-enters through the same flags, so a scratch state
    // directory survives the spawn.
    let mut args = vec!["serve".to_string()];
    for (flag, value) in [
        (
            "--config",
            common.config.as_ref().map(|p| p.display().to_string()),
        ),
        (
            "--state",
            common.state.as_ref().map(|p| p.display().to_string()),
        ),
        (
            "--watches",
            common.watches.as_ref().map(|p| p.display().to_string()),
        ),
        ("--log-dir", common.log_dir.clone()),
    ] {
        if let Some(value) = value {
            args.push(flag.to_string());
            args.push(value);
        }
    }
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    if let Err(error) =
        inkling::serve::ensure_running(&serve_config(config, common), &borrowed, why)
    {
        tracing::warn!(%error, "could not start a server");
    }
}

/// Claude Code's hook payload. Only the fields inkling needs — the rest is
/// somebody else's business and may change without notice, which is why the
/// tool's own input and result stay untyped.
#[derive(serde::Deserialize)]
struct HookInput {
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    tool_name: String,
    #[serde(default)]
    tool_input: serde_json::Value,
    #[serde(default)]
    tool_response: serde_json::Value,
}

fn field<'a>(value: &'a serde_json::Value, key: &str) -> &'a str {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
}

/// How long a hook waits for its payload.
///
/// Claude Code writes the payload and closes stdin, so this never fires in
/// practice. It is here because the failure it prevents is the worst shape a
/// hook has: one that hangs the session waiting for a write nobody is going to
/// make. Generous, because the cost of firing early is a hook that does
/// nothing.
const PAYLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Do `work`, or give up on it once `budget` has passed.
///
/// On a thread, because the alternative is bounding every step a poll is made
/// of — a token subprocess, an HTTP request, one `acli` per issue — and the
/// sum of those bounds is not a bound worth having. The process exits either
/// way, so work still in flight costs one thread until then; abandoning a poll
/// loses its own request and nothing else, since every write it might still
/// make lands under the store's lock by rename.
fn within<T: Send + 'static>(
    budget: std::time::Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (done, waiting) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(work());
    });
    waiting.recv_timeout(budget).ok()
}

/// The hook payload, or nothing within `timeout`.
fn payload<R: std::io::Read + Send + 'static>(
    source: R,
    timeout: std::time::Duration,
) -> Result<String, String> {
    within(timeout, move || {
        let mut raw = String::new();
        let mut source = source;
        std::io::Read::read_to_string(&mut source, &mut raw)
            .map(|_| raw)
            .map_err(|e| e.to_string())
    })
    .unwrap_or_else(|| {
        Err(format!(
            "no hook payload on stdin within {}",
            humantime::format_duration(timeout)
        ))
    })
}

/// Whether this process is a hook of the bridge's own `claude -p` child. That
/// child is a Claude Code session running the user's hooks; inkling's must do
/// nothing there, or every Slack read subscribes a throwaway session and the
/// watch it was reading for gets a sibling.
fn inside_bridge(marker: Option<std::ffi::OsString>) -> bool {
    marker.is_some_and(|value| !value.is_empty())
}

/// Run a hook, and cost the calling session nothing whatever happens.
///
/// A hook's exit code is a decision in Claude Code's protocol, not a status: 2
/// on `UserPromptSubmit` erases the user's prompt, and 2 on `SessionStart`
/// blocks the session. So the answer is always 0, and that has to hold for the
/// failures the body below never gets to see — a config that won't parse, and a
/// panic.
fn swallowing(event: HookEvent, run: impl FnOnce() -> Result<(), String>) -> i32 {
    // The default hook writes to stderr, which is noise in somebody's
    // transcript; whoever is debugging this is reading the log.
    std::panic::set_hook(Box::new(|panicked| {
        tracing::error!(%panicked, "panicked");
    }));
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
    let _ = std::panic::take_hook();

    match ran {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(event = event.as_str(), error, "hook failed"),
        // The panic itself was logged by the hook above.
        Err(_) => tracing::error!(event = event.as_str(), "hook panicked"),
    }
    0
}

/// How long a prompt hook will spend looking upstream before answering with
/// what it already has.
///
/// A real poll of 14 references measured 1.65s (docs/watching.md), so this
/// covers the honest case and cuts the pathological one: a `gh auth token`
/// waiting on a keychain, an `acli` that never returns. Giving up costs a
/// refresh, not a message — the feed is complete either way, and a resident
/// server is what is supposed to be paying for this.
const REFRESH_BUDGET: std::time::Duration = std::time::Duration::from_secs(3);

/// Look upstream on the prompt path, or don't, and say which.
fn refresh_within(
    budget: std::time::Duration,
    config: Config,
    path: &std::path::Path,
    refresh: TimeDelta,
) {
    let path = path.to_path_buf();
    let polled = within(budget, move || {
        let store = inkling::adapters::watches_json::WatchStore::new(path);
        poll(&config, &store, &SystemClock, Some(refresh), None, "prompt")
    });
    match polled {
        Some(Ok(_)) => {}
        Some(Err(error)) => tracing::warn!(%error, "could not refresh watches"),
        None => tracing::warn!(
            budget_ms = budget.as_millis() as u64,
            "gave up refreshing watches; answering with what is already known"
        ),
    }
}

/// Do what a hook asks, or say why not. The caller swallows the error.
fn hook(event: HookEvent, config: &Config, common: &Common) -> Result<(), String> {
    let raw = payload(std::io::stdin(), PAYLOAD_TIMEOUT)?;
    let input: HookInput = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    if input.session_id.is_empty() {
        return Err("the hook payload carried no session_id".to_string());
    }

    let clock = SystemClock;
    let watches = watch_store(common);
    match event {
        HookEvent::SessionStart => {
            subscribe(&input.session_id, config, common)?;
            Ok(())
        }
        HookEvent::SessionEnd => {
            unsubscribe(&input.session_id, common)?;
            Ok(())
        }
        HookEvent::PostTool => post_tool(&input, config, common),
        HookEvent::Prompt => {
            // The pull leg, and the reason push is allowed to be lossy: if a
            // message was held or refused, this is where it turns up.
            let refresh = TimeDelta::seconds(60);
            if watches.read().due(clock.now(), refresh) {
                refresh_within(REFRESH_BUDGET, config.clone(), watches.path(), refresh);
            }
            let due = watches
                .update(clock.now(), |file| {
                    file.take(&input.session_id, clock.now())
                })
                .map_err(|e| e.to_string())?;
            // Nothing to say means nothing printed: this runs on every
            // prompt, and the common case must add nothing to it. The same
            // wording a push uses, so news reads the same whichever way it
            // arrived.
            for event in &due {
                println!("{}", inkling::push::message(event));
            }
            Ok(())
        }
    }
}

/// How long the `post-tool` hook will wait on GitHub to say which pull
/// requests a push updated. The hook runs async, so a session never waits on
/// this; it is here so a hung `gh auth token` or request can't keep the
/// process alive, like every other hook path.
const PUSH_LOOKUP_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

type Noticed = (
    Vec<(inkling::domain::model::Reference, &'static str)>,
    Vec<inkling::domain::detect::Pushed>,
);

/// What a tool call was about: from Bash, a pull request named by URL, an
/// issue named to `acli`, a branch `git push` updated, or a message
/// `chat.postMessage` posted; from the Slack MCP send tool, the message it
/// posted.
///
/// Cheapest checks first, and in this order: this runs after every tool call,
/// and the overwhelming majority of them have nothing to do with any of it.
fn noticed(input: &HookInput, config: &Config) -> Noticed {
    use inkling::domain::{detect, slack};
    if input.tool_name != "Bash" {
        let send = slack_claude::ToolNames::new(config.slack.mcp.as_ref()).send;
        let posted = match input.tool_name == send {
            true => slack::posted_via_tool(&input.tool_input, &input.tool_response),
            false => None,
        };
        return (
            posted.map(|r| (r, "slack send")).into_iter().collect(),
            Vec::new(),
        );
    }
    let command = field(&input.tool_input, "command");
    if !detect::worth_reading(command) && !command.contains(slack::POST_METHOD) {
        return (Vec::new(), Vec::new());
    }
    let stdout = field(&input.tool_response, "stdout");
    let output = format!("{stdout}\n{}", field(&input.tool_response, "stderr"));
    let mut touched = detect::touched(command, &output, &config.github.hosts);
    touched.extend(slack::posted_via_api(command, stdout).map(|r| (r, slack::POST_METHOD)));
    let pushed = detect::pushed(command, &output, &config.github.hosts);
    (touched, pushed)
}

/// Watch what a tool call was about — see `noticed`.
///
/// A push is matched against this session's own watches first, which costs
/// nothing and is the common case — the second push to a branch. Only a
/// branch none of them has, and that GitHub didn't just offer to open a pull
/// request for, is looked up.
fn post_tool(input: &HookInput, config: &Config, common: &Common) -> Result<(), String> {
    use inkling::domain::detect;
    let (touched, pushed) = noticed(input, config);
    if touched.is_empty() && pushed.is_empty() {
        return Ok(());
    }

    let clock = SystemClock;
    let watches = watch_store(common);
    // The session that will actually be pushed to, falling back to the one
    // the payload names — never `cli`, since a watch made by a session
    // belongs to it even if nothing is attached yet.
    let owner = reader(None, &input.session_id, &watches.read());
    let deadline = (clock.now() + inkling::adapters::watches_json::WATCH_TTL).trunc_subsecs(0);
    let (mut fresh, stirred, unknown) = watches
        .update(clock.now(), |file| {
            let mut fresh = Vec::new();
            for (reference, why) in &touched {
                let known = file
                    .watches
                    .get(&reference.to_string())
                    .is_some_and(|watch| watch.owners.contains(&owner));
                file.watch(reference, &owner, deadline, clock.now());
                if !known {
                    fresh.push((reference.clone(), *why));
                }
            }
            let (stirred, unknown) = file.stir_pushed(&pushed, &owner, deadline, clock.now());
            (fresh, stirred, unknown)
        })
        .map_err(|e| e.to_string())?;
    for (reference, branch) in &stirred {
        tracing::info!(%reference, owner, branch, "watch stirred by a push");
    }
    for push in pushed.iter().filter(|push| push.invited) {
        tracing::info!(
            repo = push.repository(),
            branch = push.branch,
            "push has no pull request yet; nothing to watch"
        );
    }

    // Found by asking, so bounded by what one tool call may register.
    let room = detect::MAX_REFERENCES.saturating_sub(touched.len());
    let found: Vec<_> = look_up_pushes(config, unknown)
        .into_iter()
        .take(room)
        .collect();
    if !found.is_empty() {
        let looked_up = watches
            .update(clock.now(), |file| {
                let mut new = Vec::new();
                for (reference, head) in &found {
                    let known = file
                        .watches
                        .get(&reference.to_string())
                        .is_some_and(|watch| watch.owners.contains(&owner));
                    file.watch(reference, &owner, deadline, clock.now());
                    file.remember_head(reference, head.clone());
                    if !known {
                        new.push((reference.clone(), "git push"));
                    }
                }
                new
            })
            .map_err(|e| e.to_string())?;
        fresh.extend(looked_up);
    }

    // Only what is new. The same command run in a loop refreshes a deadline
    // and says nothing, or the log becomes the loop.
    for (reference, why) in &fresh {
        tracing::info!(
            %reference,
            owner,
            tool = input.tool_name,
            matched = why,
            deadline = stamp(deadline),
            "watch registered automatically"
        );
    }
    if touched.is_empty() && stirred.is_empty() && found.is_empty() {
        return Ok(());
    }
    // Deliberately no baseline read here: the server's next tick reads it — a
    // watch that has never been read is due immediately — and if the
    // reference turns out to be already merged, that first reading drops the
    // watch without emitting anything.
    ensure_serving(config, common, "post-tool");
    Ok(())
}

/// The open pull requests these pushes updated, asked of GitHub in one
/// request, or nothing within `PUSH_LOOKUP_BUDGET`. Failures are logged: a
/// push nobody could look up watches nothing, as it did before.
fn look_up_pushes(
    config: &Config,
    pushes: Vec<inkling::domain::detect::Pushed>,
) -> Vec<(
    inkling::domain::model::Reference,
    inkling::domain::watch::Head,
)> {
    if pushes.is_empty() {
        return Vec::new();
    }
    let started = std::time::Instant::now();
    let github = config.github.clone();
    let branches: Vec<String> = pushes
        .iter()
        .map(|push| format!("{}:{}", push.repository(), push.branch))
        .collect();
    let asked = within(PUSH_LOOKUP_BUDGET, move || {
        let token = Secret::resolve(github.token_env.as_deref(), github.token_command.as_deref())
            .map_err(|e| e.to_string())?;
        GitHubProvider::from_config(&github, token)
            .pushed_to(&pushes)
            .map_err(|e| e.to_string())
    });
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match asked {
        Some(Ok(found)) => {
            tracing::info!(
                branches = branches.join(","),
                found = found.len(),
                elapsed_ms,
                "looked up the pull requests a push updated"
            );
            found
                .into_iter()
                .map(|(_, reference, head)| (reference, head))
                .collect()
        }
        Some(Err(error)) => {
            tracing::warn!(branches = branches.join(","), %error, "could not look up a push");
            Vec::new()
        }
        None => {
            tracing::warn!(
                branches = branches.join(","),
                budget_ms = PUSH_LOOKUP_BUDGET.as_millis() as u64,
                "gave up looking up a push"
            );
            Vec::new()
        }
    }
}

/// Attach a session's inbox. Reports whether it replaced an address that was
/// already there — a resumed session, with a new process and a new socket.
///
/// One local write under the lock, plus a spawn when there is something to
/// serve. A SessionEnd hook runs on a short timeout in a session that is
/// closing, and its sibling here has to be the same shape.
fn subscribe(id: &str, config: &Config, common: &Common) -> Result<bool, String> {
    let socket = std::env::var_os(SOCKET_ENV).ok_or_else(|| {
        format!("{SOCKET_ENV} is not set — subscribe runs inside a Claude Code session")
    })?;
    let socket = PathBuf::from(socket);

    // Captured now, while the session that asked is known to be the one at
    // the path. Socket paths are per-pid and outlive their session, so
    // without this a stored address can later point at a stranger's inbox.
    let identity = inkling::adapters::session_socket::SocketIdentity::capture(&socket)
        .map_err(|e| format!("{}: {e}", socket.display()))?;
    let address = inkling::adapters::session_socket::SessionAddress::new(
        &socket,
        std::env::var(TOKEN_ENV).ok().map(Secret::new),
    )
    .expecting(identity);

    let clock = SystemClock;
    let replaced = watch_store(common)
        .update(clock.now(), |file| file.attach(id, address, clock.now()))
        .map_err(|e| e.to_string())?;

    tracing::info!(
        subscriber = id,
        socket = %socket.display(),
        reattached = replaced,
        "subscribed"
    );
    ensure_serving(config, common, "subscribe");
    Ok(replaced)
}

/// Detach the address, and only the address. Whatever this session is still
/// owed stays owed, so ending one loses nothing.
fn unsubscribe(id: &str, common: &Common) -> Result<bool, String> {
    let clock = SystemClock;
    let detached = watch_store(common)
        .update(clock.now(), |file| file.detach(id))
        .map_err(|e| e.to_string())?;
    tracing::info!(subscriber = id, detached, "unsubscribed");
    Ok(detached)
}

/// clap wants an owned error; `Cursor`'s own is already one.
fn parse_cursor(arg: &str) -> Result<inkling::domain::watch::Cursor, String> {
    arg.parse()
        .map_err(|e: inkling::domain::watch::UnknownCursor| e.to_string())
}

/// A timestamp for a log line: RFC 3339, whole seconds. Nothing inkling
/// schedules is meaningful below one.
fn stamp(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// A duration at the resolution someone waiting actually cares about.
fn humanize(wait: TimeDelta) -> String {
    let minutes = wait.num_minutes();
    match minutes {
        0 => "under a minute".to_string(),
        m if m < 60 => format!("{m}m"),
        m if m < 60 * 24 => format!("{}h{:02}m", m / 60, m % 60),
        m => format!("{}d", m / (60 * 24)),
    }
}

/// Columns of the terminal, or a reasonable width when output isn't a tty.
fn terminal_width() -> usize {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // Ask the terminal directly; COLUMNS isn't exported to children by most
    // shells, so it can't be relied on.
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
    match ok && size.ws_col > 0 {
        true => size.ws_col as usize,
        false => 100,
    }
}

/// Cut to `max` characters, marking that something was removed. Counts
/// characters rather than bytes so a multi-byte title isn't split mid-glyph.
fn ellipsize(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", kept.trim_end())
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

fn watch_store(common: &Common) -> inkling::adapters::watches_json::WatchStore {
    inkling::adapters::watches_json::WatchStore::new(
        common
            .watches
            .clone()
            .unwrap_or_else(inkling::adapters::watches_json::WatchStore::default_path),
    )
}

/// The same targets `wait` takes, plus Jira issues. Channels are refused for
/// the same reason: there is no such thing as watching one.
///
/// Returns what resolved and what didn't, rather than stopping at the first
/// failure: watching is per-reference, so a typo in one argument must not
/// cost the caller the watch on the one beside it. The refusals are still
/// reported, and still change the exit code.
fn watch_targets(
    targets: &[String],
    hosts: &[String],
    known: &[inkling::domain::model::Reference],
) -> (Vec<inkling::domain::model::Reference>, Vec<String>) {
    let mut references = Vec::new();
    let mut refused = Vec::new();
    for target in targets {
        match watch_target(target, hosts, known) {
            Ok(reference) => references.push(reference),
            Err(problem) => refused.push(problem),
        }
    }
    (references, refused)
}

/// Resolve one target to watch.
///
/// Its own function rather than `classify` only because the answer is one
/// reference and a channel is refused; the rule that completes a short
/// reference is `complete`, shared with every other command.
fn watch_target(
    target: &str,
    hosts: &[String],
    known: &[inkling::domain::model::Reference],
) -> Result<inkling::domain::model::Reference, String> {
    use inkling::domain::detect::{Target, parse_target};

    match parse_target(target, hosts)? {
        Target::Channel(channel) => Err(format!(
            "{channel}: watch takes pull requests, Jira issues and Slack messages, not channels"
        )),
        Target::Reference(reference) => Ok(reference),
        Target::Partial {
            owner,
            repo,
            number,
        } => complete(owner, repo, number, hosts, known),
    }
}

/// Bring the watches up to date: whatever the schedule says is due, and with
/// `refresh` nothing younger than it, under the poll lock. `None` is `watch`
/// reading what it just registered, which must not lose to a lock.
fn poll(
    config: &Config,
    store: &inkling::adapters::watches_json::WatchStore,
    clock: &SystemClock,
    refresh: Option<TimeDelta>,
    slack: Option<&dyn inkling::ports::MessageProvider>,
    why: &str,
) -> Result<inkling::watch::Polled, String> {
    let token = Secret::resolve(
        config.github.token_env.as_deref(),
        config.github.token_command.as_deref(),
    )
    .map_err(|e| e.to_string())?;

    let poller = inkling::watch::Poller {
        store,
        github: &GitHubProvider::from_config(&config.github, token),
        jira: &inkling::adapters::jira_acli::JiraProvider::new(),
        slack,
        clock,
    };
    match refresh {
        Some(refresh) => poller.poll_if_stale(refresh, why),
        None => poller.poll(TimeDelta::zero(), why),
    }
    .map_err(|e| e.to_string())
}

/// Slack messages, read through the claude → Slack MCP bridge. Building it
/// costs nothing; who the reader is in Slack is found out, and cached in
/// `state.json`, only when a message is actually read.
fn slack_threads(
    config: &Config,
    state: &std::path::Path,
) -> Result<inkling::adapters::slack_thread::SlackThreads<slack_claude::ClaudeRunner>, String> {
    let slack = slack_claude::from_config(config, None).map_err(|e| e.to_string())?;
    let config = config.clone();
    let state = state.to_path_buf();
    Ok(inkling::adapters::slack_thread::SlackThreads::new(
        slack,
        move || {
            if let Some(id) = &config.slack.user_id {
                return Some(id.clone());
            }
            let slack = slack_claude::from_config(&config, None).ok()?;
            resolve_identity(&slack, &JsonStore::new(state.clone()))
                .map_err(|error| {
                    tracing::warn!(
                        error,
                        "could not find out who you are in slack; your own replies will be pushed"
                    )
                })
                .ok()
        },
    ))
}

/// One line's worth of where a watched thing stands.
fn standing(standing: Option<&inkling::domain::watch::Standing>) -> String {
    use inkling::domain::watch::Standing;
    match standing {
        None => "unread".to_string(),
        Some(Standing::PullRequest { state, checks }) => match checks {
            Some(checks) => format!("{state}, checks {checks}"),
            None => state.to_string(),
        },
        Some(Standing::Issue { status, .. }) => status.clone(),
        Some(Standing::Message { reactions, replies }) => {
            let mut said = vec![format!("{} replies", replies.len())];
            said.extend(reactions.iter().map(|(emoji, n)| format!(":{emoji}: {n}")));
            said.join(", ")
        }
    }
}

/// Say so when something the caller just asked for was over before the watch
/// began.
///
/// A finished reference has its watch dropped by the first reading, so it
/// never appears in the list — and "watching nothing" is a poor answer to
/// "watch this pull request".
///
/// Only what was asked for. One poll reads every live watch, so it finishes
/// other readers' as well, and their housekeeping is the log's business rather
/// than this command's output.
fn finished_already(
    asked: &[inkling::domain::model::Reference],
    polled: &inkling::watch::Polled,
    watches: &inkling::adapters::watches_json::Watches,
) -> Vec<String> {
    polled
        .sightings
        .iter()
        .filter(|(reference, _)| {
            asked.contains(reference) && !watches.watches.contains_key(&reference.to_string())
        })
        .map(|(reference, sighting)| {
            format!(
                "{reference}: already {} — nothing left to watch",
                standing(Some(&sighting.standing))
            )
        })
        .collect()
}

fn watch_out(
    watches: &inkling::adapters::watches_json::Watches,
    owner: &str,
    now: DateTime<Utc>,
    json: bool,
) -> Result<(), String> {
    let mut live: Vec<_> = watches
        .watches
        .values()
        .filter(|watch| watch.deadline > now)
        .collect();
    live.sort_by_key(|watch| watch.reference.to_string());

    if json {
        let out: Vec<_> = live
            .iter()
            .map(|watch| {
                serde_json::json!({
                    "reference": watch.reference.to_string(),
                    "standing": standing(watch.seen.as_ref().map(|seen| &seen.standing)),
                    "since": watch.since,
                    "deadline": watch.deadline,
                    "title": watch.seen.as_ref().map(|s| s.title.clone()).unwrap_or_default(),
                    "owners": watch.owners,
                    "mine": watch.owners.contains(owner),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
        );
        return Ok(());
    }

    if live.is_empty() {
        println!("watching nothing");
        return Ok(());
    }

    let rows: Vec<_> = live
        .iter()
        .map(|watch| {
            (
                watch.reference.to_string(),
                standing(watch.seen.as_ref().map(|seen| &seen.standing)),
                humanize(watch.deadline - now),
                watch
                    .seen
                    .as_ref()
                    .map(|s| s.title.clone())
                    .unwrap_or_default(),
            )
        })
        .collect();
    let (name_width, standing_width) = widths(&rows);
    let budget = terminal_width()
        .saturating_sub(2 + name_width + 2 + standing_width + 2 + 8 + 2)
        .max(24);

    // The list is everything inkling is watching, for everyone; a mark says
    // which of them are yours. One character rather than a column, because
    // the answer is a yes or a no.
    for (watch, (name, state, left, title)) in live.iter().zip(&rows) {
        let mine = match watch.owners.contains(owner) {
            true => "*",
            false => " ",
        };
        let title = ellipsize(title, budget);
        println!("{mine} {name:<name_width$}  {state:<standing_width$}  {left:<8}  {title}");
    }
    Ok(())
}

/// Column widths for a table whose first two columns are arbitrary-length.
fn widths(rows: &[(String, String, String, String)]) -> (usize, usize) {
    let width = |pick: fn(&(String, String, String, String)) -> &String| {
        rows.iter().map(|row| pick(row).len()).max().unwrap_or(0)
    };
    (width(|row| &row.0), width(|row| &row.1))
}

/// Nothing changed, nothing printed — in every format. A hook injects this
/// into a prompt, so silence has to be the cheap and literal default.
fn events_out(
    owner: &str,
    events: &[inkling::domain::watch::Event],
    json: bool,
    ndjson: bool,
) -> Result<(), String> {
    if events.is_empty() {
        return Ok(());
    }

    if json || ndjson {
        let rows: Vec<_> = events
            .iter()
            .map(|event| {
                serde_json::json!({
                    "seq": event.seq,
                    "cursor": inkling::domain::watch::Cursor::new(owner, event.seq).to_string(),
                    "at": event.at,
                    "reference": event.reference.to_string(),
                    "axis": event.axis.as_str(),
                    "from": event.from,
                    "to": event.to,
                    "title": event.title,
                    "detail": event.detail,
                    "job": event.job,
                })
            })
            .collect();
        // Every row carries the cursor it leaves you at, so a self-managing
        // caller resumes from the last one it handled without building a
        // token itself — and a partly-processed batch resumes correctly.
        // Text output doesn't: it goes into a prompt, where a token is noise.
        for line in match ndjson {
            true => rows
                .iter()
                .map(|row| serde_json::to_string(row).map_err(|e| e.to_string()))
                .collect::<Result<Vec<_>, _>>()?,
            false => vec![serde_json::to_string_pretty(&rows).map_err(|e| e.to_string())?],
        } {
            println!("{line}");
        }
        return Ok(());
    }

    let rows: Vec<_> = events
        .iter()
        .map(|event| {
            (
                event.reference.to_string(),
                event.axis.to_string(),
                format!("{} → {}", event.from, event.to),
                event.detail.clone().unwrap_or_else(|| event.title.clone()),
            )
        })
        .collect();
    let (name_width, axis_width) = widths(&rows);
    let move_width = rows
        .iter()
        .map(|row| row.2.chars().count())
        .max()
        .unwrap_or(0);
    let budget = terminal_width()
        .saturating_sub(name_width + 2 + axis_width + 2 + move_width + 2)
        .max(24);

    for (name, axis, moved, detail) in &rows {
        // chars, not bytes: the arrow is three bytes wide and one column.
        let pad = " ".repeat(move_width - moved.chars().count());
        println!(
            "{name:<name_width$}  {axis:<axis_width$}  {moved}{pad}  {}",
            ellipsize(detail, budget)
        );
    }
    Ok(())
}

/// What the resident server is doing, if anything.
///
/// "Running" comes from the lock, because that is the only claim the kernel
/// stands behind; the record beside it is what the last server wrote about
/// itself, and may be a corpse's.
fn serve_out(status: &inkling::serve::Status) {
    let local = |t: DateTime<Utc>| {
        t.with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M %Z")
            .to_string()
    };
    match (status.running, &status.record) {
        (true, Some(record)) => {
            println!("serving     yes (pid {})", record.pid);
            println!("  uptime    {}", humanize(Utc::now() - record.started_at));
            println!("  ticks     {} ({} failed)", record.ticks, record.failures);
            println!(
                "  last tick {}",
                record.last_tick_at.map_or("never".into(), local)
            );
            if let Some(error) = &record.last_error {
                println!("  last error {error}");
            }
        }
        (true, None) => println!("serving     yes (no record yet)"),
        (false, Some(record)) => println!(
            "serving     no (last one ran {} ticks, until {})",
            record.ticks,
            record.last_tick_at.map_or("never".into(), local)
        ),
        (false, None) => println!("serving     no"),
    }
}

/// What the last day cost and produced, read back out of the log.
///
/// "Is inkling being used, and is it working" was five greps: the counts are
/// spread across a hook, a resident server and a terminal, and only the log
/// sees all three. This is the one place that adds them up. Every line here
/// names the `inkling logs -g` that shows the detail behind it.
/// What each transition caught cost upstream — the number that says whether
/// the reading schedule is too eager. Two significant figures at most: it is a
/// ratio of two daily counts.
fn per_transition(reads: usize, transitions: usize) -> String {
    if transitions == 0 {
        return String::new();
    }
    let ratio = reads as f64 / transitions as f64;
    match ratio >= 10.0 {
        true => format!(" — {ratio:.0} reads per transition"),
        false => format!(" — {ratio:.1} reads per transition"),
    }
}

fn last_day_out(directory: &std::path::Path, now: DateTime<Utc>) {
    let from = (now - TimeDelta::hours(24))
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    let lines = obs::since(directory, &from);
    let tally = obs::Tally::of(lines.iter().map(String::as_str));

    println!("last 24h    {} sessions subscribed", tally.subscribed);
    println!(
        "  watches   {} registered, {} transitions",
        tally.watches, tally.transitions
    );
    println!(
        "  delivered {} pushed, {} withheld, {} pulled",
        tally.pushed, tally.withheld, tally.pulls
    );
    println!(
        "  upstream  {} pull request reads in {} github queries, {} jira reads{}",
        tally.pull_requests_read,
        tally.github_queries,
        tally.issues_read,
        per_transition(
            tally.pull_requests_read + tally.issues_read,
            tally.transitions
        )
    );
    println!(
        "  trouble   {} hook failures, {} references unread, {} errors",
        tally.hook_failures, tally.unread, tally.errors
    );
}

/// Who is reading, where each one has got to, and whether it can be pushed
/// to. Never the token, and never the socket path — neither is anybody's
/// business at a glance.
fn subscribers_out(watches: &inkling::adapters::watches_json::Watches, now: DateTime<Utc>) {
    // A reader nothing can reach — detached, watching nothing, owed nothing —
    // is a cursor waiting out its retention, and a row about it says the same
    // nothing every day. Counted, so it is still accounted for.
    let (live, dormant): (Vec<_>, Vec<_>) = watches
        .subscribers
        .iter()
        .partition(|(id, _)| watches.reachable(id, now));

    println!(
        "subscribers {}{}",
        live.len(),
        match dormant.len() {
            0 => String::new(),
            n => format!(" ({n} dormant)"),
        }
    );
    for (id, subscriber) in live {
        let watching = watches
            .watches
            .values()
            .filter(|watch| watch.deadline > now && watch.owners.contains(id))
            .count();
        println!(
            "  {:<8}  {:<8}  cursor {:<5}  watching {watching:<3}  last seen {}",
            short(id),
            match subscriber.address.is_some() {
                true => "attached",
                false => "detached",
            },
            subscriber.cursor,
            humanize(now - subscriber.seen_at),
        );
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

fn wait_out(report: &inkling::wait::Report, json: bool) -> Result<(), String> {
    let waited = report.waited.as_secs();

    if json {
        let out = serde_json::json!({
            "outcome": report.outcome.as_str(),
            "until": report.until.to_string(),
            // Whole seconds: a poll-interval-derived duration has no finer
            // precision, and printing more would invent some.
            "waited_seconds": waited,
            "polls": report.polls,
            "references": report.observations.iter().map(|o| serde_json::json!({
                "reference": o.reference.to_string(),
                "state": o.state.as_str(),
                "checks": o.checks.map(|c| c.as_str()),
                "title": o.title,
            })).collect::<Vec<_>>(),
            "vanished": report.vanished.iter().map(ToString::to_string).collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
        );
        return Ok(());
    }

    for reference in &report.vanished {
        println!("{reference}: not found, or not visible with these credentials");
    }

    let width = report
        .observations
        .iter()
        .map(|o| o.reference.to_string().len())
        .max()
        .unwrap_or(0);
    let budget = terminal_width()
        .saturating_sub(2 + 10 + 2 + width + 2)
        .max(24);

    for observation in &report.observations {
        let name = observation.reference.to_string();
        // The check state is the answer when that's what was asked for, and
        // noise otherwise.
        let state = match (report.until, observation.checks) {
            (inkling::domain::wait::Until::Checks, Some(checks)) => checks.to_string(),
            (inkling::domain::wait::Until::Checks, None) => "no checks".to_string(),
            _ => observation.state.to_string(),
        };
        let title = ellipsize(&observation.title, budget);
        println!("  {state:<10}  {name:<width$}  {title}");
    }

    println!();
    println!(
        "{} after {} ({} {})",
        report.outcome.as_str().replace('_', " "),
        humantime::format_duration(std::time::Duration::from_secs(waited)),
        report.polls,
        if report.polls == 1 { "poll" } else { "polls" }
    );
    Ok(())
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
                .format("%Y-%m-%d %H:%M %Z");
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
        tracking(&report.tracking);
    }

    for reference in &report.unmatched {
        println!("{reference}: nothing tracked references it");
    }

    // "checked 0" alone reads the same whether nothing was due or nothing was
    // found, so say how many were eligible and when the next one comes up.
    let checked = match report.next_check {
        Some(next) => {
            let wait = next
                .signed_duration_since(Utc::now())
                .max(TimeDelta::zero());
            format!(
                "checked {}/{} (next in {})",
                report.checked,
                report.live,
                humanize(wait)
            )
        }
        None => format!("checked {}/{}", report.checked, report.live),
    };

    let verb = if dry_run { "would add" } else { "added" };
    println!(
        "discovered {} · {checked} · {verb} {} · skipped {} · failed {} · tracking {} ({} done)",
        report.discovered,
        report.added.len(),
        report.skipped,
        report.failed,
        report.tracked,
        report.terminal,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        Config, HookEvent, HookInput, classify, ellipsize, finished_already, inside_bridge,
        noticed, payload, swallowing, wait_targets, watch_target, watch_targets, within,
    };
    use inkling::adapters::store_json::Snapshot;
    use inkling::adapters::watches_json::Watches;
    use inkling::domain::watch::{Sighting, Standing};
    use inkling::watch::Polled;

    fn hosts() -> Vec<String> {
        vec!["github.com".to_string()]
    }

    #[test]
    fn the_starter_config_is_a_config_inkling_accepts() {
        inkling::config::Config::parse(super::STARTER_CONFIG, "config.toml").unwrap();
    }

    #[test]
    fn reads_per_transition_claims_no_more_precision_than_it_has() {
        use super::per_transition;
        assert_eq!(per_transition(120, 0), "", "nothing caught, no ratio");
        assert_eq!(per_transition(1000, 3), " — 333 reads per transition");
        assert_eq!(per_transition(20, 3), " — 6.7 reads per transition");
    }

    /// One argument, put through every command that takes a reference.
    ///
    /// The level the dogfood finding was at: each command was tested against
    /// its own idea of what a reference is, so `watch` learning a form and
    /// `wait` not learning it looked green both times.
    fn each_command(
        target: &str,
        known: &[inkling::domain::model::Reference],
    ) -> Vec<(&'static str, Result<String, String>)> {
        let one = [target.to_string()];
        let names = |references: Vec<inkling::domain::model::Reference>| {
            references
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        };
        vec![
            (
                "watch",
                watch_target(target, &hosts(), known).map(|r| r.to_string()),
            ),
            ("wait", wait_targets(&one, &hosts(), known).map(names)),
            (
                "run",
                classify(&one, &hosts(), known).map(|(_, references)| names(references)),
            ),
        ]
    }

    #[test]
    fn a_watch_lasts_as_long_as_its_events_are_kept_however_it_was_made() {
        // `inkling watch` takes its default from clap, the post-tool hook from
        // the constant: two spellings of one number.
        use clap::Parser;
        let cli = super::Cli::try_parse_from(["inkling", "watch", "42"]).unwrap();
        let super::Command::Watch { ttl, .. } = cli.command else {
            unreachable!()
        };
        let default = inkling::adapters::watches_json::WATCH_TTL;
        assert_eq!(ttl, default.to_std().unwrap());
        assert!(default <= inkling::adapters::watches_json::RETENTION);
    }

    #[test]
    fn ringing_the_server_needs_neither_a_server_nor_a_config() {
        // `make install` rings after installing, and must never fail for it.
        use clap::Parser;
        let dir = std::env::temp_dir().join(format!("inkling-ring-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.toml");
        std::fs::write(&config, "this is [not toml").unwrap();
        let watches = dir.join("watches.json");

        let cli = super::Cli::try_parse_from([
            "inkling".as_ref(),
            "serve".as_ref(),
            "--ring".as_ref(),
            "--config".as_ref(),
            config.as_os_str(),
            "--watches".as_ref(),
            watches.as_os_str(),
        ])
        .unwrap();
        let started = std::time::Instant::now();
        assert_eq!(super::run(&cli), Ok(0));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(
            !dir.join("serve.wake").exists(),
            "ringing makes no doorbell; a server does"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_command_accepts_the_same_reference() {
        // Found by dogfooding: `watch dpep/inkling#7` was accepted seconds
        // before `wait dpep/inkling#7` was refused. `owner/repo#N` names a
        // pull request completely once the host defaults, and which command
        // you are running has nothing to do with it.
        for target in [
            "https://github.com/dpep/inkling/pull/3",
            "github.com/dpep/inkling/pull/3",
            "github.com/dpep/inkling#3",
            "dpep/inkling#3",
        ] {
            for (command, resolved) in each_command(target, &[]) {
                assert_eq!(
                    resolved.as_deref(),
                    Ok("github.com/dpep/inkling#3"),
                    "{command} {target}"
                );
            }
        }
    }

    #[test]
    fn a_shorter_form_still_needs_something_watched_to_match() {
        // The other half of the one rule, and the reason it isn't "accept
        // everything": `inkling#3` leaves out the owner, and guessing one
        // would watch somebody else's pull request. Only here does a short
        // form select among what is already known.
        for target in ["inkling#3", "#3", "3"] {
            for (command, resolved) in each_command(target, &[]) {
                let error = resolved.expect_err(target);
                assert!(error.contains("nothing watched"), "{command}: {error}");
            }
        }
    }

    #[test]
    fn a_shorter_form_selects_what_is_already_watched() {
        // Found by dogfooding: short forms matched only what the Slack
        // reactions tracked, so with those parked `wait 3` never worked.
        let now = chrono::Utc::now();
        let mut watches = Watches::default();
        watches.watch(&pr(3), "cli", now + chrono::TimeDelta::hours(1), now);
        let known = super::known(&Snapshot::default(), &watches);

        for target in ["inkling#3", "#3", "3"] {
            for (command, resolved) in each_command(target, &known) {
                assert_eq!(
                    resolved.as_deref(),
                    Ok("github.com/dpep/inkling#3"),
                    "{command} {target}"
                );
            }
        }
    }

    #[test]
    fn one_unresolvable_reference_does_not_stop_the_others() {
        // Found by dogfooding: a typo in the second argument cost the watch
        // on the first one, which was the whole reason for running it.
        let targets = ["dpep/inkling#3".to_string(), "42".to_string()];
        let (references, refused) = watch_targets(&targets, &hosts(), &[]);

        assert_eq!(references.len(), 1);
        assert_eq!(references[0].to_string(), "github.com/dpep/inkling#3");
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert!(refused[0].contains("#42"), "{refused:?}");
    }

    fn pr(number: u64) -> inkling::domain::model::Reference {
        inkling::domain::model::Reference::GitHubPullRequest {
            host: "github.com".into(),
            owner: "dpep".into(),
            repo: "inkling".into(),
            number,
        }
    }

    /// A reading of a merged pull request — one whose watch the poll dropped.
    fn merged(number: u64) -> (inkling::domain::model::Reference, Sighting) {
        (
            pr(number),
            Sighting {
                standing: Standing::PullRequest {
                    state: inkling::domain::model::State::Merged,
                    checks: None,
                },
                title: "Add the serve lifecycle".into(),
                failing: None,
                head: None,
                by: Default::default(),
            },
        )
    }

    #[test]
    fn watching_one_thing_reports_only_on_that_thing() {
        // Found by dogfooding: watching a new pull request printed "already
        // merged — nothing left to watch" for two older ones the same poll
        // happened to finish. One poll reads every live watch, so it does
        // somebody else's housekeeping too; the log already says so.
        let polled = Polled {
            sightings: vec![merged(3), merged(5), merged(7)],
            ..Polled::default()
        };
        let said = finished_already(&[pr(7)], &polled, &Watches::default());

        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].starts_with("github.com/dpep/inkling#7: already merged"));
    }

    #[test]
    fn a_hook_payload_that_never_arrives_does_not_hang_the_session() {
        // Claude Code always closes stdin, but a hook whose worst case is a
        // hung session has the wrong worst case.
        struct Silent;

        impl std::io::Read for Silent {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                std::thread::sleep(std::time::Duration::from_secs(2));
                Ok(0)
            }
        }

        let error = payload(Silent, std::time::Duration::from_millis(20)).unwrap_err();
        assert!(error.contains("within 20ms"), "{error}");
    }

    #[test]
    fn work_that_overruns_its_budget_is_left_behind() {
        // The refresh a prompt hook pays for: a `gh auth token` that never
        // returns, or an `acli` that hangs, must cost the prompt the budget
        // and not a second more.
        let started = std::time::Instant::now();
        let answer = within(std::time::Duration::from_millis(50), || {
            std::thread::sleep(std::time::Duration::from_secs(30));
            "too late"
        });

        assert_eq!(answer, None);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn work_that_finishes_in_time_comes_back() {
        assert_eq!(
            within(std::time::Duration::from_secs(5), || "in time"),
            Some("in time")
        );
    }

    #[test]
    fn a_hook_that_fails_costs_the_session_nothing() {
        // Exit codes are decisions in the hook protocol: 2 on
        // UserPromptSubmit erases the prompt, 2 on SessionStart blocks the
        // session. Nothing inkling can get wrong is worth either.
        assert_eq!(
            swallowing(HookEvent::Prompt, || Err("the config won't parse".into())),
            0
        );
    }

    #[test]
    fn a_hook_that_panics_costs_the_session_nothing() {
        assert_eq!(
            swallowing(HookEvent::Prompt, || panic!("somewhere deep")),
            0
        );
    }

    #[test]
    fn a_hook_payload_is_read_whole() {
        let json = r#"{"session_id":"abc","tool_name":"Bash"}"#;
        let read = payload(
            std::io::Cursor::new(json),
            std::time::Duration::from_secs(5),
        );
        assert_eq!(read.as_deref(), Ok(json));
    }

    #[test]
    fn hooks_do_nothing_inside_the_bridge_s_own_claude() {
        assert!(inside_bridge(Some("1".into())));
        assert!(!inside_bridge(None));
        assert!(!inside_bridge(Some("".into())), "set but empty is unset");
    }

    fn tool_call(json: serde_json::Value) -> HookInput {
        serde_json::from_value(json).unwrap()
    }

    fn slack_message(channel: &str, ts: &str) -> inkling::domain::model::Reference {
        inkling::domain::model::Reference::SlackMessage {
            channel: channel.into(),
            ts: ts.into(),
            thread: None,
        }
    }

    #[test]
    fn the_slack_send_tool_watches_what_it_posted() {
        let input = tool_call(serde_json::json!({
            "session_id": "s",
            "tool_name": "mcp__claude_ai_Slack__slack_send_message",
            "tool_input": {"channel_id": "C0123ABCD", "message": "ready for review?"},
            "tool_response": [{"type": "text", "text": "Message sent: https://acme.slack.com/archives/C0123ABCD/p1786596050245879"}]
        }));
        let (touched, pushed) = noticed(&input, &Config::default());
        assert_eq!(
            touched,
            [(
                slack_message("C0123ABCD", "1786596050.245879"),
                "slack send"
            )]
        );
        assert!(pushed.is_empty());
    }

    #[test]
    fn another_tool_or_another_server_s_send_watches_nothing() {
        let input = tool_call(serde_json::json!({
            "session_id": "s",
            "tool_name": "mcp__other__slack_send_message",
            "tool_input": {"channel_id": "C0123ABCD", "message": "hi"},
            "tool_response": "https://acme.slack.com/archives/C0123ABCD/p1786596050245879"
        }));
        assert!(noticed(&input, &Config::default()).0.is_empty());
    }

    #[test]
    fn a_curl_to_chat_post_message_watches_what_it_posted() {
        let input = tool_call(serde_json::json!({
            "session_id": "s",
            "tool_name": "Bash",
            "tool_input": {"command": "curl -s https://slack.com/api/chat.postMessage -H @auth -d @msg.json"},
            "tool_response": {"stdout": r#"{"ok":true,"channel":"C0123ABCD","ts":"1786596050.245879"}"#, "stderr": ""}
        }));
        assert_eq!(
            noticed(&input, &Config::default()).0,
            [(
                slack_message("C0123ABCD", "1786596050.245879"),
                "chat.postMessage"
            )]
        );
    }

    #[test]
    fn a_slack_message_can_be_watched_but_not_waited_on() {
        let permalink = "https://acme.slack.com/archives/C0123ABCD/p1786596050245879";
        assert_eq!(
            watch_target(permalink, &hosts(), &[]),
            Ok(slack_message("C0123ABCD", "1786596050.245879"))
        );
        let error = wait_targets(&[permalink.to_string()], &hosts(), &[]).unwrap_err();
        assert!(error.contains("inkling watch"), "{error}");
    }

    #[test]
    fn a_channel_is_refused_rather_than_watched() {
        let error = watch_target("#eng", &hosts(), &[]).unwrap_err();
        assert!(error.contains("not channels"), "{error}");
    }

    #[test]
    fn long_titles_are_cut_rather_than_wrapped() {
        // A wrapped row stops being a row: the columns stop lining up, and
        // the tail reads as a stray label on its own line.
        // Cut at the limit rather than a word boundary — a table column
        // wants as much of the title as fits.
        assert_eq!(ellipsize("Tighten result ranking", 10), "Tighten r…");
        assert_eq!(ellipsize("short", 10), "short");
        assert_eq!(ellipsize("exactly-10", 10), "exactly-10");
    }

    #[test]
    fn cutting_counts_characters_not_bytes() {
        // Truncating by byte would split a multi-byte glyph in half.
        assert_eq!(ellipsize("émoji—dash ok", 6), "émoji…");
    }
}
