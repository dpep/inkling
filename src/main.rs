//! `inkling` — annotate Slack messages with the state of what they link to.

use chrono::{DateTime, SubsecRound, TimeDelta, Utc};
use clap::{Args, Parser, Subcommand};
use inkling::adapters::github::GitHubProvider;
use inkling::adapters::slack_claude;
use inkling::adapters::store_json::JsonStore;
use inkling::adapters::watches_json::{TERMINAL, Unwatched};
use inkling::config::Config;
use inkling::config_edit;
use inkling::obs;
use inkling::ports::{Clock, StoreError, SystemClock};
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
        /// browse URL. Slack: a message's permalink, or a whole channel as
        /// `#name`, its id, or its link.
        targets: Vec<String>,
        /// Stop watching after this long, e.g. 2h, 24h, 7d, 30d. A standing
        /// channel watch usually wants days.
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
    /// Stop pushing to this session. It stays attached, so its watches keep
    /// being read and the prompt hook still shows what moved.
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
            println!("github     {}", credentials(&config));
            Ok(0)
        }
        Command::Subscribe { session } => {
            let id = session_id(session.as_deref())?;
            let replaced = subscribe(&id, &config, &cli.common, true)?;
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
            match unsubscribe(&id, &cli.common, true)? {
                true => println!("unsubscribed {}", short(&id)),
                false => println!("{}: not subscribed; nothing to unsubscribe", short(&id)),
            }
            Ok(0)
        }
        // Handled above, before the config load they must not depend on.
        Command::Hook { .. } | Command::Serve { ring: true } => unreachable!(),
        Command::Serve { ring: false } => {
            let watches = watch_store(&cli.common);
            let github = GitHubOnDemand(&config.github);
            let jira = inkling::adapters::jira_acli::JiraProvider::new();
            // Fails here rather than on the worker, where no one would see it.
            slack_threads(&config, store.path())?;
            let clock = SystemClock;
            // Slack is the worker's, so a bridge call never holds up a tick.
            let poller = || inkling::watch::Poller {
                store: &watches,
                github: &github,
                jira: &jira,
                slack: None,
                clock: &clock,
            };
            let work = inkling::push::Tick {
                poller: poller(),
                sessions: &inkling::push::RealSessions,
                clock: &clock,
            };
            let serving = serve_config(&config, &cli.common);
            let interval = serving.interval;
            let state = store.path().to_path_buf();
            // Built on its own thread: the reader caches who you are in a
            // RefCell, which can't be shared.
            let slack_worker = |beside: &inkling::serve::Beside| {
                let Ok(slack) = slack_threads(&config, &state) else {
                    return;
                };
                let poller = inkling::watch::Poller {
                    slack: Some(&slack),
                    ..poller()
                };
                while !beside.stopping() {
                    match poller.poll_slack("serve-slack", &|| beside.stopping()) {
                        Ok(polled) if !polled.events.is_empty() => beside.wake(),
                        Ok(_) => {}
                        Err(error) => tracing::warn!(%error, "could not read slack watches"),
                    }
                    beside.nap(interval);
                }
            };

            match inkling::serve::run(serving, &work, Some(&slack_worker))
                .map_err(|e| e.to_string())?
            {
                inkling::serve::Outcome::AlreadyRunning => Ok(0),
                inkling::serve::Outcome::Served(_) => Ok(0),
            }
        }
        Command::Status => {
            let now = Utc::now();
            // A newer build's store reads as empty, and zeros here would send
            // someone to watch everything again.
            let watched = watch_store(&cli.common).read_current();
            match &watched {
                Ok(watched) => {
                    let live = watched
                        .watches
                        .values()
                        .filter(|w| w.deadline > now)
                        .count();
                    let paused = watched
                        .watches
                        .values()
                        .filter(|w| w.deadline > now && watched.paused(w))
                        .count();
                    println!("watching    {live}");
                    if paused > 0 {
                        println!("  paused    {paused} (every reader's session has ended)");
                    }
                    println!("  events    {} in the last day", watched.events.len());
                }
                Err(error) => println!("watching    unknown: {}", refused_store(error)),
            }

            println!();
            serve_out(&inkling::serve::read_status(&serve_config(
                &config,
                &cli.common,
            )));

            if let Ok(watched) = &watched {
                println!();
                subscribers_out(watched, now);
            }

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
            let mut unreadable = false;
            let owner = reader(None, TERMINAL, &watches.read());

            if !targets.is_empty() {
                let known = known(&watches.read());
                let references;
                let bridge = slack_claude::from_config(&config).map_err(|e| e.to_string())?;
                let channel = |handle: &str| slack_channel(handle, &bridge, &store);
                (references, refused) =
                    watch_targets(targets, &config.github.hosts, &known, &channel);
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
                    let these = Reading::These(&references);
                    match poll(&config, &watches, &clock, these, Some(&slack), "watch") {
                        Ok(polled) => {
                            for line in finished_already(&references, &polled, &watches.read()) {
                                println!("{line}");
                            }
                            for line in unread(&references, &polled) {
                                println!("could not read {line}");
                                unreadable = true;
                            }
                            let typos = typos(&references, &polled, &watches.read());
                            if !typos.is_empty() {
                                watches
                                    .update(clock.now(), |file| {
                                        for reference in &typos {
                                            file.unwatch(reference, &owner);
                                        }
                                    })
                                    .map_err(|e| e.to_string())?;
                            }
                            for reference in &typos {
                                tracing::info!(%reference, owner, "watch dropped; not there");
                                println!("not watching {reference}");
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

            let listed = watches.read_current().map_err(|e| refused_store(&e))?;
            watch_out(&listed, &owner, clock.now(), *json)?;
            // The good references are watched either way; the exit code is
            // where a script learns that one of its arguments was not, or
            // couldn't be read.
            Ok(match refused.is_empty() && !unreadable {
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

            let known = known(&watches.read());
            let bridge = slack_claude::from_config(&config).map_err(|e| e.to_string())?;
            let channel = |handle: &str| slack_channel(handle, &bridge, &store);
            let (references, refused) =
                watch_targets(targets, &config.github.hosts, &known, &channel);
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
                let serving = serve_config(&config, &cli.common);
                let slack = slack_unless_serving(&config, &serving, store.path())?;
                let slack = slack
                    .as_ref()
                    .map(|s| s as &dyn inkling::ports::MessageProvider);
                if let Err(error) = poll(
                    &config,
                    &watches,
                    &clock,
                    Reading::Stale(refresh),
                    slack,
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
                    let owner = reader_named(&file, &cursor.owner)?;
                    let due = inkling::domain::watch::addressed(&file.events, cursor.seq, &owner)
                        .into_iter()
                        .cloned()
                        .collect();
                    (owner, due)
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

            // Read-only, and no lock: a wait never writes the watches. It
            // reads them only to expand short references like `inkling#2`.
            let known = known(&watch_store(&cli.common).read());
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
    now: DateTime<Utc>,
) -> Result<String, String> {
    let known = store.load().map_err(|e| e.to_string())?;
    if let Some(id) = known.user_id {
        return Ok(id);
    }
    let retry = inkling::adapters::watches_json::SLACK_READ_FLOOR;
    if let Some(at) = known.user_id_failed_at.filter(|at| now - *at < retry) {
        return Err(format!(
            "it failed at {at}; asking again after {}",
            at + retry
        ));
    }
    let me = match slack.whoami() {
        Ok(me) => me,
        Err(error) => {
            let _ = store.update(|snapshot| snapshot.user_id_failed_at = Some(now));
            return Err(error.to_string());
        }
    };

    let cached = store.update(|snapshot| {
        snapshot.user_id = Some(me.id.clone());
        snapshot.user_id_failed_at = None;
    });
    if let Err(error) = cached {
        tracing::warn!(%error, "could not cache who you are in slack; asking again next time");
    }
    Ok(me.id)
}

/// Everything a short reference can select among: what is watched.
fn known(
    watches: &inkling::adapters::watches_json::Watches,
) -> Vec<inkling::domain::model::Reference> {
    watches
        .watches
        .values()
        .map(|w| w.reference.clone())
        .collect()
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
            Some(host) => Ok(inkling::domain::model::Reference::pull_request(
                host, &owner, &repo, number,
            )),
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
    if let Some(channel) = references
        .iter()
        .find(|r| matches!(r, inkling::domain::model::Reference::SlackChannel { .. }))
    {
        return Err(format!("{channel}: wait takes pull requests, not channels"));
    }
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

/// The reader a cursor names: its whole id, or the start of one, which is
/// what `inkling status` shows — so long as only one reader starts that way.
fn reader_named(
    watches: &inkling::adapters::watches_json::Watches,
    name: &str,
) -> Result<String, String> {
    if watches.knows(name) {
        return Ok(name.to_string());
    }
    let matching: Vec<&String> = watches
        .subscribers
        .keys()
        .filter(|id| id.starts_with(name))
        .collect();
    match matching.as_slice() {
        [one] => Ok(one.to_string()),
        [] => Err(format!(
            "{name}: no reader's id is or starts with that. One that never subscribed, \
             or was silent for {} days, has no cursor; start again with --subscriber",
            inkling::adapters::watches_json::SUBSCRIBER_RETENTION.num_days(),
        )),
        many => Err(format!(
            "{name}: {} readers' ids start with that; give more of it",
            many.len()
        )),
    }
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
    /// `SessionStart` only: startup, resume, clear, compact or fork.
    #[serde(default)]
    source: String,
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
        poll(
            &config,
            &store,
            &SystemClock,
            Reading::Stale(refresh),
            None,
            "prompt",
        )
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
            subscribe(
                &input.session_id,
                config,
                common,
                unmutes_at_start(&input.source),
            )?;
            Ok(())
        }
        HookEvent::SessionEnd => {
            // Not asked for: a session that is still there (a resume) gets
            // attached again by its next hook.
            unsubscribe(&input.session_id, common, false)?;
            Ok(())
        }
        HookEvent::PostTool => {
            revive(&input.session_id, config, common);
            post_tool(&input, config, common)
        }
        HookEvent::Prompt => {
            revive(&input.session_id, config, common);
            // The pull leg, and the reason push is allowed to be lossy: if a
            // message was held or refused, this is where it turns up.
            let refresh = TimeDelta::seconds(60);
            let serve = serve_config(config, common);
            if refreshes_at_prompt(&serve, &watches.read(), clock.now(), refresh) {
                refresh_within(REFRESH_BUDGET, config.clone(), watches.path(), refresh);
            }
            let due = watches
                .update(clock.now(), |file| {
                    file.take(&input.session_id, clock.now())
                })
                .map_err(|e| e.to_string())?;
            if let Some(news) = news(&due) {
                println!("{news}");
            }
            Ok(())
        }
    }
}

/// Whether the prompt hook looks upstream itself: only when something is due
/// and no server is ticking. A server keeps readings fresh, and a refresh
/// abandoned at its budget leaves its `acli` children running, since what
/// times them out is a thread of this short-lived process.
fn refreshes_at_prompt(
    serve: &inkling::serve::Config,
    watches: &inkling::adapters::watches_json::Watches,
    now: DateTime<Utc>,
    floor: TimeDelta,
) -> bool {
    inkling::watch::due_without_bridge(watches, now, floor)
        && !inkling::serve::read_status(serve).ticking(now, serve.interval)
}

/// Whether `SessionStart` undoes an unsubscribe: starting or resuming the
/// session does; a compaction, the same session carrying on, doesn't.
fn unmutes_at_start(source: &str) -> bool {
    matches!(source, "startup" | "resume")
}

/// What a prompt owes the session, worded and grouped as a push is, so news
/// reads the same whichever way it arrived. `None` when nothing is owed: this
/// runs on every prompt, and the common case must add nothing to it.
fn news(due: &[inkling::domain::watch::Event]) -> Option<String> {
    let due: Vec<_> = due.iter().collect();
    (!due.is_empty()).then(|| inkling::push::digest(&due))
}

/// How long the `post-tool` hook will wait on GitHub to say which pull
/// requests a push updated. The hook runs async, so a session never waits on
/// this; it is here so a hung `gh auth token` or request can't keep the
/// process alive, like every other hook path.
const PUSH_LOOKUP_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

type Noticed = (
    Vec<(inkling::domain::model::Reference, &'static str)>,
    Vec<inkling::domain::detect::Pushed>,
    Vec<inkling::domain::watch::Receipt>,
);

/// What a tool call was about: from Bash, a pull request named by URL, an
/// issue named to `acli`, a branch `git push` updated, or a message
/// `chat.postMessage` posted; from the Slack MCP send tool, the message it
/// posted. A post, or a reaction added through the MCP react tool, is also a
/// receipt: what the session did itself.
///
/// Cheapest checks first, and in this order: this runs after every tool call,
/// and the overwhelming majority of them have nothing to do with any of it.
fn noticed(input: &HookInput, config: &Config) -> Noticed {
    use inkling::domain::{detect, slack};
    if input.tool_name != "Bash" {
        let tools = slack_claude::ToolNames::new(config.slack.mcp.as_ref());
        if input.tool_name == tools.react {
            let reacted = slack::reacted_via_tool(&input.tool_input);
            return (Vec::new(), Vec::new(), reacted.into_iter().collect());
        }
        let posted = match input.tool_name == tools.send {
            true => slack::posted_via_tool(&input.tool_input, &input.tool_response),
            false => None,
        };
        let receipts = posted.iter().filter_map(slack::receipt).collect();
        return (
            posted.map(|r| (r, "slack send")).into_iter().collect(),
            Vec::new(),
            receipts,
        );
    }
    let command = field(&input.tool_input, "command");
    if !detect::worth_reading(command) && !command.contains(slack::POST_METHOD) {
        return (Vec::new(), Vec::new(), Vec::new());
    }
    let stdout = field(&input.tool_response, "stdout");
    let output = format!("{stdout}\n{}", field(&input.tool_response, "stderr"));
    let mut touched = detect::touched(command, &output, &config.github.hosts);
    let posted = slack::posted_via_api(command, stdout);
    let receipts = posted.iter().filter_map(slack::receipt).collect();
    touched.extend(posted.map(|r| (r, slack::POST_METHOD)));
    let pushed = detect::pushed(command, &output, &config.github.hosts);
    (touched, pushed, receipts)
}

/// Watch what a tool call was about — see `noticed`.
///
/// A push is matched against this session's own watches first, which costs
/// nothing and is the common case — the second push to a branch. Only a
/// branch none of them has, and that GitHub didn't just offer to open a pull
/// request for, is looked up.
fn post_tool(input: &HookInput, config: &Config, common: &Common) -> Result<(), String> {
    use inkling::domain::detect;
    let (touched, pushed, receipts) = noticed(input, config);
    if touched.is_empty() && pushed.is_empty() && receipts.is_empty() {
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
                    .get(&reference.key())
                    .is_some_and(|watch| watch.owners.contains(&owner));
                file.watch(reference, &owner, deadline, clock.now());
                if !known {
                    fresh.push((reference.clone(), *why));
                }
            }
            for receipt in &receipts {
                file.receipt(&owner, receipt, clock.now());
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
                        .get(&reference.key())
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
/// `unmute` undoes an `inkling unsubscribe`; otherwise a muted session stays so.
///
/// One local write under the lock, plus a spawn when there is something to
/// serve. A SessionEnd hook runs on a short timeout in a session that is
/// closing, and its sibling here has to be the same shape.
fn subscribe(id: &str, config: &Config, common: &Common, unmute: bool) -> Result<bool, String> {
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
        .update(clock.now(), |file| {
            if unmute {
                file.unmute(id);
            }
            file.attach(id, address, clock.now())
        })
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

/// Attach again a session the store holds no address for, muted if it was.
/// Only a live session fires hooks, and a probe can be wrong: macOS refuses a
/// connection to a listener whose backlog is full exactly as it does to a
/// dead one.
fn revive(id: &str, config: &Config, common: &Common) {
    let Some(socket) = std::env::var_os(SOCKET_ENV) else {
        return;
    };
    if !watch_store(common).lost_address(id, std::path::Path::new(&socket)) {
        return;
    }
    match subscribe(id, config, common, false) {
        Ok(_) => tracing::info!(
            subscriber = id,
            "a session with no address fired a hook; attached again"
        ),
        Err(error) => tracing::warn!(subscriber = id, error, "could not attach again"),
    }
}

/// `asked` is a person's `inkling unsubscribe`: a mute, which keeps the
/// address. Otherwise the session ended: detach the address, and only the
/// address, so whatever it is still owed stays owed.
fn unsubscribe(id: &str, common: &Common, asked: bool) -> Result<bool, String> {
    let clock = SystemClock;
    let detached = watch_store(common)
        .update(clock.now(), |file| match asked {
            true => file.unsubscribe(id),
            false => file.detach(id),
        })
        .map_err(|e| e.to_string())?;
    tracing::info!(subscriber = id, detached, asked, "unsubscribed");
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

/// Why the watch store can't be shown, in the terms that say what to do.
fn refused_store(error: &StoreError) -> String {
    match error {
        StoreError::FutureVersion { found } => format!(
            "the watch store was written by a newer build (v{found}); this is v{}",
            inkling::adapters::watches_json::VERSION
        ),
        other => other.to_string(),
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

/// The same targets `wait` takes, plus Jira issues, Slack messages and Slack
/// channels. A channel is named explicitly — `#name`, its id, or its link —
/// and `channel` turns that into a reference; a bare word is refused, since a
/// typo would otherwise cost a bridge call and watch nothing.
///
/// Returns what resolved and what didn't, rather than stopping at the first
/// failure: watching is per-reference, so a typo in one argument must not
/// cost the caller the watch on the one beside it. The refusals are still
/// reported, and still change the exit code.
fn watch_targets(
    targets: &[String],
    hosts: &[String],
    known: &[inkling::domain::model::Reference],
    channel: &dyn Fn(&str) -> Result<inkling::domain::model::Reference, String>,
) -> (Vec<inkling::domain::model::Reference>, Vec<String>) {
    let mut references = Vec::new();
    let mut refused = Vec::new();
    for target in targets {
        match watch_target(target, hosts, known, channel) {
            Ok(reference) => references.push(reference),
            Err(problem) => refused.push(problem),
        }
    }
    (references, refused)
}

/// Resolve one target to watch.
///
/// Its own function rather than `classify` only because the answer is one
/// reference and a channel is resolved; the rule that completes a short
/// reference is `complete`, shared with every other command.
fn watch_target(
    target: &str,
    hosts: &[String],
    known: &[inkling::domain::model::Reference],
    channel: &dyn Fn(&str) -> Result<inkling::domain::model::Reference, String>,
) -> Result<inkling::domain::model::Reference, String> {
    use inkling::domain::detect::{Target, parse_target};
    use inkling::domain::model::Reference;

    // The form inkling prints a named channel in, which `unwatch` takes back.
    if let Some(name) = target.trim().strip_prefix("slack:#") {
        return channel(&format!("#{name}"));
    }
    match parse_target(target, hosts)? {
        Target::Channel(handle)
            if handle.starts_with('#') || slack_claude::looks_like_channel_id(&handle) =>
        {
            channel(&handle)
        }
        Target::Channel(word) => Err(format!(
            "{word}: watch takes pull requests, Jira issues, Slack messages and \
             Slack channels — a channel is #name, its id, or its link"
        )),
        Target::Reference(Reference::SlackChannel { channel: id, .. }) => channel(&id),
        Target::Reference(reference) => Ok(reference),
        Target::Partial {
            owner,
            repo,
            number,
        } => complete(owner, repo, number, hosts, known),
    }
}

/// A Slack channel by `#name` or by id, named when that is known.
///
/// The name is display only: a watch is keyed by the id, so `#general` and
/// `C0E48AHSN` are one watch whichever came first. An id takes the name the
/// cache has for it. A name nobody has resolved yet costs one bridge call, kept in
/// `state.json` with the rest of the channel cache.
fn slack_channel<R: slack_claude::Runner>(
    handle: &str,
    slack: &slack_claude::SlackClaude<R>,
    store: &JsonStore,
) -> Result<inkling::domain::model::Reference, String> {
    use inkling::domain::model::Reference;

    let snapshot = store.load().unwrap_or_default();
    let Some(name) = handle.strip_prefix('#') else {
        let name = snapshot
            .channels
            .iter()
            .find(|(_, id)| *id == handle)
            .map(|(name, _)| name.clone());
        return Ok(Reference::SlackChannel {
            channel: handle.to_string(),
            name,
        });
    };
    let name = name.to_lowercase();
    slack.seed_channels(snapshot.channels.clone());
    let id = slack
        .resolve_channel(&name)
        .map_err(|error| format!("#{name}: {error}"))?;
    if snapshot.channels.get(&name) != Some(&id) {
        // Best-effort: a cache that can't be written costs a lookup next time.
        let _ = store.update(|s| s.channels.insert(name.clone(), id.clone()));
    }
    Ok(Reference::SlackChannel {
        channel: id,
        name: Some(name),
    })
}

/// Which watches a `poll` reads.
enum Reading<'a> {
    /// Whatever the schedule says is due, and nothing younger than this,
    /// under the poll lock.
    Stale(TimeDelta),
    /// What `watch` just registered, which must not lose to a lock.
    These(&'a [inkling::domain::model::Reference]),
}

/// Bring the watches up to date.
fn poll(
    config: &Config,
    store: &inkling::adapters::watches_json::WatchStore,
    clock: &SystemClock,
    reading: Reading,
    slack: Option<&dyn inkling::ports::MessageProvider>,
    why: &str,
) -> Result<inkling::watch::Polled, String> {
    let poller = inkling::watch::Poller {
        store,
        github: &GitHubOnDemand(&config.github),
        jira: &inkling::adapters::jira_acli::JiraProvider::new(),
        slack,
        clock,
    };
    match reading {
        Reading::Stale(refresh) => poller.poll_if_stale(refresh, why),
        Reading::These(these) => poller.poll_these(these, why),
    }
    .map_err(|e| e.to_string())
}

/// What `events` reads Slack with: nothing while a server is serving. The
/// server reads it anyway, and a bridge call would hold the command for a
/// minute or more.
fn slack_unless_serving(
    config: &Config,
    serving: &inkling::serve::Config,
    state: &std::path::Path,
) -> Result<Option<inkling::adapters::slack_thread::SlackThreads<slack_claude::ClaudeRunner>>, String>
{
    match inkling::serve::read_status(serving).running {
        true => Ok(None),
        false => slack_threads(config, state).map(Some),
    }
}

/// GitHub, with the token resolved at each lookup: only a due pull request
/// pays for `gh auth token`, a token that can't be had fails GitHub alone, and
/// a resident server picks up a login made after it started.
struct GitHubOnDemand<'a>(&'a inkling::config::GitHub);

impl inkling::ports::StatusProvider for GitHubOnDemand<'_> {
    fn lookup(
        &self,
        references: &[inkling::domain::model::Reference],
    ) -> Result<Vec<inkling::ports::Lookup>, inkling::ports::ProviderError> {
        let token = Secret::resolve(self.0.token_env.as_deref(), self.0.token_command.as_deref())
            .map_err(|e| inkling::ports::ProviderError::Transport(e.to_string()))?;
        GitHubProvider::from_config(self.0, token).lookup(references)
    }
}

/// Slack messages, read through the claude → Slack MCP bridge. Building it
/// costs nothing; who the reader is in Slack is found out, and cached in
/// `state.json`, only when a message is actually read.
fn slack_threads(
    config: &Config,
    state: &std::path::Path,
) -> Result<inkling::adapters::slack_thread::SlackThreads<slack_claude::ClaudeRunner>, String> {
    let slack = slack_claude::from_config(config).map_err(|e| e.to_string())?;
    let config = config.clone();
    let state = state.to_path_buf();
    Ok(inkling::adapters::slack_thread::SlackThreads::new(
        slack,
        move || {
            if let Some(id) = &config.slack.user_id {
                return Some(id.clone());
            }
            let slack = slack_claude::from_config(&config).ok()?;
            resolve_identity(&slack, &JsonStore::new(state.clone()), SystemClock.now())
                .map_err(|error| {
                    tracing::warn!(
                        error,
                        "could not find out who you are in slack; a channel message mentioning you will not count as a reply"
                    )
                })
                .ok()
        },
    ))
}

/// One line's worth of where a watched thing stands, counted in the words a
/// transition on it would use.
fn standing(standing: Option<&inkling::domain::watch::Standing>) -> String {
    use inkling::domain::watch::Standing;
    use inkling::push::{COMMENT, MESSAGE, REACTION, REPLY, tally};
    let counted = |count: u64, noun| tally(count, noun, false, &[], &[]);
    match standing {
        None => "unread".to_string(),
        Some(Standing::PullRequest {
            state,
            checks,
            comments,
        }) => {
            let mut said = vec![state.to_string()];
            said.extend(checks.map(|checks| format!("checks {checks}")));
            said.extend(comments.as_ref().map(|c| counted(c.count, COMMENT)));
            said.join(", ")
        }
        Some(Standing::Issue { status, .. }) => status.clone(),
        Some(Standing::Message { reactions, replies }) => {
            let mut said = vec![counted(replies.len() as u64, REPLY)];
            said.extend(reactions.iter().map(|(emoji, n)| {
                tally(
                    u64::from(*n),
                    REACTION,
                    false,
                    &[],
                    &[&format!(":{emoji}:")],
                )
            }));
            said.join(", ")
        }
        // Since the watch began: the running count, which no line says.
        Some(Standing::Channel { count, .. }) => {
            format!("{} since watched", counted(*count, MESSAGE))
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
            asked.contains(reference) && !watches.watches.contains_key(&reference.key())
        })
        .map(|(reference, sighting)| {
            format!(
                "{reference}: already {} — nothing left to watch",
                standing(Some(&sighting.standing))
            )
        })
        .collect()
}

/// Why the references just asked for couldn't be read, if they couldn't —
/// not the other watches the same poll happened to read. One that has no line
/// of its own and wasn't read fell with its whole provider, whose line it gets.
fn unread(
    asked: &[inkling::domain::model::Reference],
    polled: &inkling::watch::Polled,
) -> Vec<String> {
    let mut own = Vec::new();
    let mut whole: Vec<(&str, Vec<String>)> = Vec::new();
    for reference in asked {
        let mine = format!("{reference}: ");
        if let Some(line) = polled.failures.iter().find(|l| l.starts_with(&mine)) {
            own.push(line.clone());
            continue;
        }
        if polled.sightings.iter().any(|(seen, _)| seen == reference) {
            continue;
        }
        let prefix = inkling::ports::ProviderError::prefix(reference);
        let Some(line) = polled.failures.iter().find(|l| l.starts_with(prefix)) else {
            continue;
        };
        match whole.iter_mut().find(|(l, _)| *l == line.as_str()) {
            Some((_, references)) => references.push(reference.to_string()),
            None => whole.push((line, vec![reference.to_string()])),
        }
    }
    own.extend(
        whole
            .into_iter()
            .map(|(line, references)| format!("{}: {line}", references.join(", "))),
    );
    own
}

/// What was just asked for and read as not there at its first reading: a
/// typo, most likely, with nothing to wait for. A watch read before keeps
/// waiting out its misses, since one reading can be wrong.
fn typos(
    asked: &[inkling::domain::model::Reference],
    polled: &inkling::watch::Polled,
    watches: &inkling::adapters::watches_json::Watches,
) -> Vec<inkling::domain::model::Reference> {
    asked
        .iter()
        .filter(|reference| polled.missing.contains(reference))
        .filter(|reference| {
            watches
                .watches
                .get(&reference.key())
                .is_some_and(|watch| watch.seen.is_none())
        })
        .cloned()
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
                    "paused": watches.paused(watch),
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
            let state = standing(watch.seen.as_ref().map(|seen| &seen.standing));
            (
                watch.reference.to_string(),
                // Not read until a reader comes back, so the standing is as of
                // when it paused — the answer to "why wasn't I told".
                match watches.paused(watch) {
                    true => format!("{state} (paused)"),
                    false => state,
                },
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
            .map(|event| event_json(owner, event))
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

    for line in event_lines(events, terminal_width()) {
        println!("{line}");
    }
    Ok(())
}

/// One event for a machine: everything stored that says what it means, and
/// the read that shows it, so a caller needn't rebuild either.
fn event_json(owner: &str, event: &inkling::domain::watch::Event) -> serde_json::Value {
    use inkling::domain::watch::Axis;
    // Names on an accumulating axis, without the "by " older events carried.
    let detail = match event.axis {
        Axis::Comments | Axis::Replies | Axis::Messages if event.detail.is_some() => {
            Some(inkling::push::names(event).join(", "))
        }
        _ => event.detail.clone(),
    };
    serde_json::json!({
        "seq": event.seq,
        "cursor": inkling::domain::watch::Cursor::new(owner, event.seq).to_string(),
        "at": event.at,
        "reference": event.reference.to_string(),
        "axis": event.axis.as_str(),
        "from": event.from,
        "to": event.to,
        "title": event.title,
        "detail": detail,
        "job": event.job,
        "partial": event.partial,
        "own": event.receipt_for.contains(owner),
        "lookup": inkling::push::more(event),
    })
}

/// The text table: what moved worded as a push words it, then the read that
/// shows it where there is one, and otherwise the title.
fn event_lines(events: &[inkling::domain::watch::Event], width: usize) -> Vec<String> {
    let rows: Vec<_> = events
        .iter()
        .map(|event| {
            let tail = match inkling::push::more(event) {
                Some(command) => format!("({command})"),
                None => event.title.clone(),
            };
            (
                event.reference.to_string(),
                event.axis.to_string(),
                inkling::push::change(event),
                tail,
            )
        })
        .collect();
    let (name_width, axis_width) = widths(&rows);
    let move_width = rows
        .iter()
        .map(|row| row.2.chars().count())
        .max()
        .unwrap_or(0);
    let budget = width
        .saturating_sub(name_width + 2 + axis_width + 2 + move_width + 2)
        .max(24);

    rows.iter()
        .zip(events)
        .map(|((name, axis, moved, tail), event)| {
            // chars, not bytes: the arrow is three bytes wide and one column.
            let pad = " ".repeat(move_width - moved.chars().count());
            // A command is never cut: half of one runs as something else.
            let tail = match inkling::push::more(event) {
                Some(_) => tail.clone(),
                None => ellipsize(tail, budget),
            };
            format!("{name:<name_width$}  {axis:<axis_width$}  {moved}{pad}  {tail}")
                .trim_end()
                .to_string()
        })
        .collect()
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

/// Every provider's reads, and the money Slack's cost, since a Slack read is
/// the only one that costs any.
fn upstream(tally: &obs::Tally) -> String {
    let slack = match tally.slack_usd > 0.0 {
        // Cents: the bridge reports more digits than a day's sum deserves.
        true => format!(
            "{} slack reads (${:.2})",
            tally.slack_reads, tally.slack_usd
        ),
        false => format!("{} slack reads", tally.slack_reads),
    };
    format!(
        "{} pull request reads in {} github queries, {} jira reads, {slack}{}",
        tally.pull_requests_read,
        tally.github_queries,
        tally.issues_read,
        per_transition(
            tally.pull_requests_read + tally.issues_read + tally.slack_reads,
            tally.transitions
        )
    )
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
    println!("  upstream  {}", upstream(&tally));
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
            "  {:<8}  {:<12}  cursor {:<5}  watching {watching:<3}  last seen {}",
            short(id),
            connection(subscriber),
            subscriber.cursor,
            humanize(now - subscriber.seen_at),
        );
    }
}

/// Whether a reader can be pushed to, and if not, why not.
fn connection(subscriber: &inkling::adapters::watches_json::Subscriber) -> &'static str {
    // "ended" is a session we saw go, which is what pauses watches;
    // "detached" never attached, so nothing says it has gone.
    match (
        subscriber.address.is_some(),
        subscriber.departed,
        subscriber.unsubscribed,
    ) {
        (true, _, true) => "unsubscribed",
        (true, _, false) => "attached",
        (false, true, _) => "ended",
        (false, false, _) => "detached",
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

#[cfg(test)]
mod tests {
    use super::{
        Config, HookEvent, HookInput, classify, ellipsize, finished_already, inside_bridge, news,
        noticed, payload, swallowing, unread, wait_targets, watch_target, watch_targets, within,
    };
    use inkling::adapters::watches_json::Watches;
    use inkling::domain::watch::{Receipt, Sighting, Standing};
    use inkling::watch::Polled;

    fn hosts() -> Vec<String> {
        vec!["github.com".to_string()]
    }

    #[test]
    fn status_tells_an_unsubscribed_session_from_one_that_ended() {
        let now = chrono::Utc::now();
        let mut watches = Watches::default();
        let address = || {
            inkling::adapters::session_socket::SessionAddress::new(
                std::path::Path::new("/tmp/s"),
                None,
            )
        };
        let connection = |w: &Watches| super::connection(&w.subscribers["s-1"]);
        watches.attach("s-1", address(), now);
        assert_eq!(connection(&watches), "attached");
        watches.unsubscribe("s-1");
        assert_eq!(connection(&watches), "unsubscribed");
        watches.detach("s-1");
        assert_eq!(connection(&watches), "ended");
    }

    #[test]
    fn only_a_return_to_the_session_undoes_an_unsubscribe() {
        // Found by the hunt: a compaction's SessionStart re-attached it.
        // A /clear needs no rule: Claude Code gives it a new session id.
        assert!(super::unmutes_at_start("startup"));
        assert!(super::unmutes_at_start("resume"));
        assert!(!super::unmutes_at_start("compact"));
        assert!(
            !super::unmutes_at_start(""),
            "an older Claude Code says nothing"
        );
    }

    #[test]
    fn the_prompt_hook_leaves_refreshing_to_a_running_server() {
        // Found by the hunt: a refresh abandoned at its budget left `acli`
        // children running, four more on every prompt.
        let dir = std::env::temp_dir().join(format!("inkling-prompt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let serve = inkling::serve::Config::in_dir(&dir);
        let now = chrono::Utc::now();
        let floor = chrono::TimeDelta::seconds(60);
        let mut watches = Watches::default();
        watches.watch(&pr(3), "s-1", now + chrono::TimeDelta::hours(1), now);

        let ticked = |ago: std::time::Duration| {
            let record = inkling::serve::Record {
                pid: 1,
                started_at: now - chrono::TimeDelta::hours(1),
                last_tick_at: Some(now - chrono::TimeDelta::from_std(ago).unwrap()),
                ticks: 1,
                failures: 0,
                last_error: None,
            };
            std::fs::write(&serve.status, serde_json::to_string(&record).unwrap()).unwrap();
        };
        let server = inkling::adapters::lock::FileLock::try_acquire(&serve.lock)
            .unwrap()
            .unwrap();
        ticked(serve.interval);
        assert!(!super::refreshes_at_prompt(&serve, &watches, now, floor));
        // Found by the hunt: a server winding down holds the lock but no
        // longer ticks, and the prompt went quiet with it.
        ticked(serve.interval * 3);
        assert!(
            super::refreshes_at_prompt(&serve, &watches, now, floor),
            "a server that stopped ticking keeps nothing fresh"
        );
        ticked(serve.interval);
        drop(server);
        assert!(
            super::refreshes_at_prompt(&serve, &watches, now, floor),
            "with no server, the prompt is all there is"
        );
    }

    #[test]
    fn the_starter_config_is_a_config_inkling_accepts() {
        inkling::config::Config::parse(super::STARTER_CONFIG, "config.toml").unwrap();
    }

    fn event(
        reference: inkling::domain::model::Reference,
        axis: inkling::domain::watch::Axis,
        from: &str,
        to: &str,
        detail: &str,
    ) -> inkling::domain::watch::Event {
        inkling::domain::watch::Event {
            seq: 1,
            at: "2026-11-17T12:00:00Z".parse().unwrap(),
            reference,
            axis,
            from: from.into(),
            to: to.into(),
            title: "Fix the thing".into(),
            detail: Some(detail.to_string()).filter(|d| !d.is_empty()),
            job: None,
            receipt_for: Default::default(),
            partial: false,
            lines: 0,
            after: None,
            audience: Default::default(),
        }
    }

    fn pr42() -> inkling::domain::model::Reference {
        inkling::domain::model::Reference::GitHubPullRequest {
            host: "github.com".into(),
            owner: "dpep".into(),
            repo: "inkling".into(),
            number: 42,
        }
    }

    #[test]
    fn events_text_says_what_a_push_says_and_the_read_that_shows_it() {
        use inkling::domain::watch::Axis;
        let events = [
            inkling::domain::watch::Event {
                partial: true,
                ..event(pr42(), Axis::Comments, "1", "41", "@bob")
            },
            event(pr42(), Axis::State, "open", "merged", "by @alice"),
        ];
        let lines = super::event_lines(&events, 200);
        assert_eq!(
            lines,
            [
                "github.com/dpep/inkling#42  comments  at least 40 comments from @bob  \
                 (gh pr view 42 --repo github.com/dpep/inkling --comments)",
                "github.com/dpep/inkling#42  state     open → merged by @alice         \
                 Fix the thing",
            ]
        );
        let untitled = inkling::domain::watch::Event {
            title: String::new(),
            ..events[1].clone()
        };
        assert!(
            super::event_lines(&[untitled], 200)
                .iter()
                .all(|line| !line.ends_with(' ')),
            "no padding after the last column"
        );
    }

    #[test]
    fn events_json_carries_what_the_text_does() {
        use inkling::domain::watch::Axis;
        let floor = inkling::domain::watch::Event {
            partial: true,
            receipt_for: ["s".to_string()].into(),
            ..event(pr42(), Axis::Replies, "1", "3", "by @alice, @bob")
        };
        let row = super::event_json("s", &floor);
        assert_eq!(row["partial"], true, "a floor, not an exact count");
        assert_eq!(row["own"], true);
        assert_eq!(row["detail"], "@alice, @bob", "the old prefix is gone");
        let state = super::event_json("s", &event(pr42(), Axis::State, "open", "merged", "by @x"));
        assert_eq!(state["detail"], "by @x", "a state's detail is its own");
        assert_eq!(state["lookup"], serde_json::Value::Null);
    }

    #[test]
    fn a_standing_counts_in_the_words_a_line_would_use() {
        let message = Standing::Message {
            reactions: [("dart".to_string(), 1)].into(),
            replies: [("1.000001".to_string(), "U1".to_string())].into(),
        };
        assert_eq!(
            super::standing(Some(&message)),
            "1 reply, 1 reaction :dart:"
        );
        let channel = Standing::Channel {
            count: 3,
            newest: Some("1786596050.245879".into()),
            arrived: Default::default(),
            more: false,
        };
        assert_eq!(super::standing(Some(&channel)), "3 messages since watched");
    }

    #[test]
    fn a_cursor_may_name_its_reader_as_status_shows_it() {
        let mut watches = Watches::default();
        let now = "2026-11-17T12:00:00Z".parse().unwrap();
        watches.take("4f2a91c0-aaaa", now);
        watches.take("4f2a91c0-bbbb", now);
        watches.take("9e1d0000-cccc", now);
        assert_eq!(
            super::reader_named(&watches, "9e1d0000"),
            Ok("9e1d0000-cccc".into())
        );
        let ambiguous = super::reader_named(&watches, "4f2a91c0").unwrap_err();
        assert!(ambiguous.contains("2 readers"), "{ambiguous}");
        let unknown = super::reader_named(&watches, "deadbeef").unwrap_err();
        assert!(!unknown.contains("was collected after"), "{unknown}");
    }

    #[test]
    fn upstream_counts_slack_reads_and_what_they_cost() {
        let tally = inkling::obs::Tally {
            pull_requests_read: 4,
            github_queries: 2,
            slack_reads: 3,
            slack_usd: 0.0234,
            ..Default::default()
        };
        assert_eq!(
            super::upstream(&tally),
            "4 pull request reads in 2 github queries, 0 jira reads, 3 slack reads ($0.02)"
        );
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
                watch_target(target, &hosts(), known, &named).map(|r| r.to_string()),
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
    fn one_pull_request_typed_in_any_case_is_one_reference() {
        // GitHub ignores case in owner and repo; two keys would be two
        // watches reporting every transition twice.
        for target in [
            "https://GitHub.com/DPep/Inkling/pull/3",
            "github.com/dpep/INKLING#3",
            "DPEP/inkling#3",
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
        let known = super::known(&watches);

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
        let (references, refused) = watch_targets(&targets, &hosts(), &[], &named);

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
                discussion: None,
                standing: Standing::PullRequest {
                    state: inkling::domain::model::State::Merged,
                    checks: None,
                    comments: None,
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
    fn watching_something_unreadable_says_why_about_it_alone() {
        let polled = Polled {
            failures: vec![
                "github.com/dpep/inkling#7: not found or not visible".into(),
                "github.com/dpep/inkling#70: rate limited".into(),
                "github unreachable".into(),
            ],
            ..Polled::default()
        };
        assert_eq!(
            unread(&[pr(7)], &polled),
            ["github.com/dpep/inkling#7: not found or not visible"]
        );
    }

    #[test]
    fn a_watch_missing_at_its_first_reading_is_a_typo_not_a_wait() {
        // Found by the hunt: a mistyped pull request was reported unreadable
        // and kept, to end as `gone` a few polls later.
        let now = chrono::Utc::now();
        let mut watches = Watches::default();
        for n in [7, 8] {
            watches.watch(&pr(n), "cli", now + chrono::TimeDelta::hours(1), now);
        }
        let (_, mut open) = merged(8);
        if let Standing::PullRequest { state, .. } = &mut open.standing {
            *state = inkling::domain::model::State::Open;
        }
        watches.record(&[(pr(8), open)], now);
        let polled = Polled {
            missing: vec![pr(7), pr(8), pr(9)],
            ..Polled::default()
        };
        assert_eq!(
            super::typos(&[pr(7), pr(8)], &polled, &watches),
            [pr(7)],
            "one that has been read before waits out its misses"
        );
    }

    #[test]
    fn watching_something_whose_whole_provider_failed_says_why() {
        let jira = |key: &str| inkling::domain::model::Reference::JiraIssue { key: key.into() };
        let polled = Polled {
            sightings: vec![merged(7)],
            failures: vec![
                "github request failed: HTTP 401".into(),
                "jira request failed: running `acli`: no answer within 5s".into(),
            ],
            ..Polled::default()
        };
        assert_eq!(
            unread(&[jira("AI-1"), jira("AI-2"), pr(7)], &polled),
            ["AI-1, AI-2: jira request failed: running `acli`: no answer within 5s"]
        );
    }

    #[test]
    fn news_pulled_at_a_prompt_reads_as_one_push() {
        use inkling::domain::watch::{Axis, Event};
        let reaction = |seq: u64, emoji: &str| Event {
            seq,
            at: chrono::Utc::now(),
            reference: inkling::domain::model::Reference::SlackMessage {
                channel: "C1".into(),
                ts: "1.000001".into(),
                thread: None,
            },
            axis: Axis::Reactions,
            from: "0".into(),
            to: "1".into(),
            title: String::new(),
            detail: Some(format!(":{emoji}:")),
            job: None,
            receipt_for: Default::default(),
            partial: false,
            lines: 0,
            after: None,
            audience: Default::default(),
        };
        assert_eq!(news(&[]), None, "nothing owed prints nothing");
        let said = news(&[reaction(1, "dart"), reaction(2, "+1")]).unwrap();
        assert_eq!(said.lines().count(), 1, "{said}");
        assert!(said.contains("2 reactions :dart: :+1:"), "{said}");
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
        let (touched, pushed, receipts) = noticed(&input, &Config::default());
        assert_eq!(
            touched,
            [(
                slack_message("C0123ABCD", "1786596050.245879"),
                "slack send"
            )]
        );
        assert!(pushed.is_empty());
        assert_eq!(
            receipts,
            [Receipt::Posted {
                channel: "C0123ABCD".into(),
                ts: "1786596050.245879".into()
            }],
            "what the session posted is its own"
        );
    }

    #[test]
    fn a_reaction_the_session_added_is_a_receipt_and_watches_nothing() {
        let input = tool_call(serde_json::json!({
            "session_id": "s",
            "tool_name": "mcp__claude_ai_Slack__slack_add_reaction",
            "tool_input": {"channel_id": "C0123ABCD", "message_ts": "1786596050.245879", "emoji": ":Eyes:"},
            "tool_response": [{"type": "text", "text": "ok"}]
        }));
        let (touched, _, receipts) = noticed(&input, &Config::default());
        assert!(touched.is_empty());
        assert_eq!(
            receipts,
            [Receipt::Reacted {
                channel: "C0123ABCD".into(),
                ts: "1786596050.245879".into(),
                emoji: "eyes".into()
            }]
        );
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
            watch_target(permalink, &hosts(), &[], &named),
            Ok(slack_message("C0123ABCD", "1786596050.245879"))
        );
        let error = wait_targets(&[permalink.to_string()], &hosts(), &[]).unwrap_err();
        assert!(error.contains("inkling watch"), "{error}");
    }

    /// Stands in for the bridge: `#eng` is C0ENG0001, an id stays unnamed.
    fn named(handle: &str) -> Result<inkling::domain::model::Reference, String> {
        Ok(match handle.strip_prefix('#') {
            Some(name) => inkling::domain::model::Reference::SlackChannel {
                channel: "C0ENG0001".into(),
                name: Some(name.into()),
            },
            None => inkling::domain::model::Reference::SlackChannel {
                channel: handle.into(),
                name: None,
            },
        })
    }

    #[test]
    fn a_channel_named_explicitly_is_watched_but_not_waited_on() {
        for (target, watched) in [
            ("#eng", "slack:#eng"),
            ("slack:#eng", "slack:#eng"),
            ("C0123ABCD", "slack:C0123ABCD"),
            (
                "https://acme.slack.com/archives/C0123ABCD",
                "slack:C0123ABCD",
            ),
        ] {
            assert_eq!(
                watch_target(target, &hosts(), &[], &named).map(|r| r.to_string()),
                Ok(watched.to_string()),
                "{target}"
            );
        }
        let error = watch_target("eng", &hosts(), &[], &named).unwrap_err();
        assert!(error.contains("#name"), "a bare word is a typo: {error}");
        for target in ["#eng", "https://acme.slack.com/archives/C0123ABCD"] {
            let error = wait_targets(&[target.to_string()], &hosts(), &[]).unwrap_err();
            assert!(error.contains("not channels"), "{error}");
        }
    }

    #[test]
    fn a_channel_cached_by_name_is_one_watch_however_it_is_named() {
        let dir = std::env::temp_dir().join(format!("inkling-channel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = super::JsonStore::new(dir.join("state.json"));
        let mut snapshot = store.load().unwrap_or_default();
        snapshot
            .channels
            .insert("general".into(), "C0E48AHSN".into());
        store.save(&snapshot).unwrap();
        // Never run: both forms are answered from the cache.
        let slack =
            super::slack_claude::SlackClaude::new(super::slack_claude::ClaudeRunner::default());

        let by_name = super::slack_channel("#General", &slack, &store).unwrap();
        let by_id = super::slack_channel("C0E48AHSN", &slack, &store).unwrap();
        assert_eq!(by_name, by_id);
        assert_eq!(by_id.to_string(), "slack:#general");
        std::fs::remove_dir_all(&dir).unwrap();
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

    #[test]
    fn a_failed_whoami_is_not_asked_again_by_the_next_process_inside_the_window() {
        use inkling::adapters::slack_claude::{Runner, RunnerError, SlackClaude};
        use std::cell::Cell;
        use std::rc::Rc;
        struct Refuses(Rc<Cell<usize>>);
        impl Runner for Refuses {
            fn run(&self, _: &str, _: &[&str], _: usize) -> Result<String, RunnerError> {
                self.0.set(self.0.get() + 1);
                // A timeout isn't retried, so no retry delay to sleep through.
                Err(RunnerError::TimedOut {
                    command: "claude".into(),
                    timeout: std::time::Duration::from_secs(180),
                })
            }
        }
        let dir = std::env::temp_dir().join(format!("inkling-whoami-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = super::JsonStore::new(dir.join("state.json"));
        let calls = Rc::new(Cell::new(0));
        // A fresh bridge each time, as each CLI process builds its own.
        let ask = |now| {
            let slack = SlackClaude::new(Refuses(calls.clone()));
            super::resolve_identity(&slack, &store, now)
        };
        let now: chrono::DateTime<chrono::Utc> = "2026-11-17T12:00:00Z".parse().unwrap();

        assert!(ask(now).is_err());
        let paid = calls.get();
        assert!(ask(now + chrono::TimeDelta::minutes(1)).is_err());
        assert_eq!(calls.get(), paid, "inside the window, nothing is paid");
        assert!(ask(now + chrono::TimeDelta::minutes(6)).is_err());
        assert!(calls.get() > paid, "asked again once it has passed");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Every message reads the same; a test only needs a reading to land.
    struct Quiet;

    impl inkling::ports::MessageProvider for Quiet {
        fn read(
            &self,
            messages: &[inkling::domain::model::Reference],
        ) -> Result<Vec<inkling::ports::MessageLookup>, inkling::ports::ProviderError> {
            Ok(messages
                .iter()
                .map(|r| (r.clone(), Ok(Default::default())))
                .collect())
        }
    }

    #[test]
    fn without_a_github_token_the_other_watches_are_still_read() {
        // A Slack-only user with no `gh` login had nothing read at all.
        let dir = std::env::temp_dir().join(format!("inkling-token-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut config =
            inkling::config::Config::parse(super::STARTER_CONFIG, "config.toml").unwrap();
        config.github.token_env = Some("INKLING_TEST_NO_SUCH_TOKEN".into());
        let store = inkling::adapters::watches_json::WatchStore::new(dir.join("watches.json"));
        let message = inkling::domain::model::Reference::SlackMessage {
            channel: "D0123ABCD".into(),
            ts: "1786596050.245879".into(),
            thread: None,
        };
        let pull = super::watch_target("dpep/inkling#3", &hosts(), &[], &|_: &str| unreachable!())
            .unwrap();
        let now = chrono::Utc::now();
        store
            .update(now, |w| {
                w.watch(&message, "test", now + chrono::TimeDelta::hours(1), now);
                w.watch(&pull, "test", now + chrono::TimeDelta::hours(1), now);
            })
            .unwrap();

        let polled = super::poll(
            &config,
            &store,
            &super::SystemClock,
            super::Reading::Stale(chrono::TimeDelta::zero()),
            Some(&Quiet),
            "test",
        )
        .unwrap();

        assert_eq!(polled.sightings.len(), 1, "the message was read");
        assert!(
            polled
                .failures
                .iter()
                .any(|f| f.contains("INKLING_TEST_NO_SUCH_TOKEN")),
            "{:?}",
            polled.failures
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn events_leaves_slack_to_a_running_server() {
        // `events` blocked on a bridge call the server was there to make.
        let dir = std::env::temp_dir().join(format!("inkling-events-slack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = inkling::config::Config::parse(super::STARTER_CONFIG, "config.toml").unwrap();
        let serving = inkling::serve::Config::in_dir(&dir);
        let state = dir.join("state.json");

        assert!(
            super::slack_unless_serving(&config, &serving, &state)
                .unwrap()
                .is_some()
        );
        let held = inkling::adapters::lock::FileLock::try_acquire(&serving.lock).unwrap();
        assert!(held.is_some());
        assert!(
            super::slack_unless_serving(&config, &serving, &state)
                .unwrap()
                .is_none()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
