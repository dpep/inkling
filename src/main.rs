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
use inkling::ports::{Clock, SystemClock};
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
    /// Where waiters coordinate. Defaults to $XDG_STATE_HOME/inkling/waits.json
    #[arg(long, global = true)]
    waits: Option<PathBuf>,
    /// Watches and their events. Defaults to $XDG_STATE_HOME/inkling/watches.json
    #[arg(long, global = true)]
    watches: Option<PathBuf>,
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
        /// Channels to search, or pull requests to check now.
        ///
        /// A channel handle or ID narrows the run to it, turning off personal
        /// mode. A pull request — full URL, `github.com/owner/repo#12`, or a
        /// bare number matching something tracked — is checked immediately,
        /// ignoring the backoff schedule.
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
        /// Pull requests to wait on — a URL, `github.com/owner/repo#12`, or a
        /// bare number matching something already tracked. Several means all
        /// of them must satisfy the condition.
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
        /// Pull requests — a URL, `github.com/owner/repo#12`, or a bare
        /// number matching something tracked — and Jira issues, as a key like
        /// `AI-123` or a browse URL.
        targets: Vec<String>,
        /// Stop watching after this long, e.g. 2h, 24h, 7d.
        #[arg(long, default_value = "24h", value_parser = humantime::parse_duration)]
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
    /// when — never a snapshot. Events carry a monotonic `seq`, so the cursor
    /// to pass to `--since` next time is the last one printed.
    ///
    /// Built for a UserPromptSubmit hook: `--subscriber <id>` keeps the
    /// cursor here, for a caller with nowhere to put one, and delivers each
    /// transition to each subscriber once. A subscriber seen for the first
    /// time starts from now rather than inheriting somebody else's history.
    ///
    /// Exits 0 whether or not anything had changed.
    Events {
        /// Only what is newer than this cursor.
        #[arg(long, conflicts_with = "subscriber")]
        since: Option<u64>,
        /// A named cursor Inkling keeps, e.g. a session id.
        #[arg(long)]
        subscriber: Option<String>,
        /// Look upstream first when the last reading is older than this.
        /// Fresh readings cost nothing but a local file read.
        #[arg(long, default_value = "60s", value_parser = humantime::parse_duration)]
        refresh: std::time::Duration,
        /// Emit the events as a JSON array.
        #[arg(long)]
        json: bool,
        /// Emit one JSON object per line.
        #[arg(long, conflicts_with = "json")]
        ndjson: bool,
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
            println!("github      {}", credentials(&config));
            Ok(0)
        }
        Command::Watch { targets, ttl, json } => {
            let watches = watch_store(&cli.common);
            let clock = SystemClock;
            let mut refused = Vec::new();

            if !targets.is_empty() {
                let snapshot = store.load().map_err(|e| e.to_string())?;
                let references;
                (references, refused) = watch_targets(targets, &config.github.hosts, &snapshot);
                for problem in &refused {
                    println!("{problem}");
                }
                let deadline = clock.now()
                    + TimeDelta::from_std(*ttl).map_err(|_| "--ttl is too long".to_string())?;

                watches
                    .update(clock.now(), |file| {
                        for reference in &references {
                            file.watch(reference, deadline, clock.now());
                            tracing::info!(%reference, %deadline, "watch registered");
                        }
                    })
                    .map_err(|e| e.to_string())?;

                // Read them now rather than leaving the baseline to whenever
                // `events` next runs: a transition in that gap would be
                // swallowed into the baseline, which is the one thing the
                // caller just asked not to happen.
                if !references.is_empty() {
                    match poll(&config, &watches, &clock, None, "watch") {
                        Ok(polled) => finished_already(&polled, &watches.read()),
                        Err(error) => {
                            println!("could not read them just now: {error}");
                            println!("  the next `inkling events` will pick them up");
                        }
                    }
                }
            }

            watch_out(&watches.read(), clock.now(), *json)?;
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

            if *all {
                let dropped = watches
                    .update(clock.now(), |file| std::mem::take(&mut file.watches).len())
                    .map_err(|e| e.to_string())?;
                println!("stopped watching {dropped}");
                return Ok(0);
            }

            let snapshot = store.load().map_err(|e| e.to_string())?;
            let (references, refused) = watch_targets(targets, &config.github.hosts, &snapshot);
            for problem in &refused {
                println!("{problem}");
            }
            let removed = watches
                .update(clock.now(), |file| {
                    references
                        .iter()
                        .map(|reference| (reference.clone(), file.unwatch(reference)))
                        .collect::<Vec<_>>()
                })
                .map_err(|e| e.to_string())?;

            for (reference, was_watched) in removed {
                match was_watched {
                    true => {
                        tracing::info!(%reference, "watch removed");
                        println!("stopped watching {reference}");
                    }
                    false => println!("{reference}: not being watched"),
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
                if let Err(error) = poll(&config, &watches, &clock, Some(refresh), "events") {
                    tracing::warn!(%error, "could not refresh watches");
                }
            }

            let events = match subscriber {
                Some(id) => watches
                    .update(clock.now(), |file| file.take(id, clock.now()))
                    .map_err(|e| e.to_string())?,
                None => {
                    let file = watches.read();
                    inkling::domain::watch::since(&file.events, since.unwrap_or(0)).to_vec()
                }
            };

            events_out(&events, *json, *ndjson)?;
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

            // Read-only, and no lock: a wait never writes the snapshot. It
            // reads it only to expand short references like `inkling#2`.
            let snapshot = store.load().map_err(|e| e.to_string())?;
            let references = wait_targets(targets, &config.github.hosts, &snapshot)?;

            let github = GitHubProvider::new(&config.github.api_url, token);
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
            let github = GitHubProvider::new(&config.github.api_url, token);
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
            let (channels, force) = classify(targets, &config.github.hosts, &snapshot)?;
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

/// Fill in the parts of a reference the caller left out, from what's already
/// tracked. Exactly one match or nothing: reacting to the wrong pull request
/// is worse than being asked for a fuller reference.
fn resolve(
    owner: &Option<String>,
    repo: &Option<String>,
    number: u64,
    snapshot: &inkling::adapters::store_json::Snapshot,
) -> Result<inkling::domain::model::Reference, String> {
    use inkling::domain::model::Reference::GitHubPullRequest;

    let mut matches: Vec<_> = snapshot
        .tracked
        .values()
        .flat_map(|t| t.references.iter())
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
            "{given}: nothing tracked references it — give the full reference, \
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
    snapshot: &inkling::adapters::store_json::Snapshot,
) -> Result<Vec<inkling::domain::model::Reference>, String> {
    let (channels, references) = classify(targets, hosts, snapshot)?;
    if let Some(channel) = channels.first() {
        return Err(format!("{channel}: wait takes pull requests, not channels"));
    }
    // A Jira issue has no merge ladder to threshold against, so `--until`
    // could say nothing about one. Watching it is the answer that exists.
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
    snapshot: &inkling::adapters::store_json::Snapshot,
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
            } => force.push(resolve(&owner, &repo, number, snapshot)?),
        }
    }
    Ok((channels, force))
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
    snapshot: &inkling::adapters::store_json::Snapshot,
) -> (Vec<inkling::domain::model::Reference>, Vec<String>) {
    let mut references = Vec::new();
    let mut refused = Vec::new();
    for target in targets {
        match watch_target(target, hosts, snapshot) {
            Ok(reference) => references.push(reference),
            Err(problem) => refused.push(problem),
        }
    }
    (references, refused)
}

/// Resolve one target to watch.
///
/// Its own loop rather than `classify`, because the completion rule differs:
/// `owner/repo#N` names a pull request completely once the host defaults, and
/// only the shorter forms need a tracked record. `run` fills every short form
/// in from what's tracked because a short form there *selects* among tracked
/// records; a watch has nothing to select among.
fn watch_target(
    target: &str,
    hosts: &[String],
    snapshot: &inkling::adapters::store_json::Snapshot,
) -> Result<inkling::domain::model::Reference, String> {
    use inkling::domain::detect::{Target, parse_target};
    use inkling::domain::model::Reference;

    match parse_target(target, hosts)? {
        Target::Channel(channel) => Err(format!(
            "{channel}: watch takes pull requests and Jira issues, not channels"
        )),
        Target::Reference(reference) => Ok(reference),
        Target::Partial {
            owner: Some(owner),
            repo: Some(repo),
            number,
        } => match hosts.first() {
            Some(host) => Ok(Reference::GitHubPullRequest {
                host: host.clone(),
                owner,
                repo,
                number,
            }),
            None => Err(format!(
                "{target}: no github host is configured — give the full reference"
            )),
        },
        Target::Partial {
            owner,
            repo,
            number,
        } => resolve(&owner, &repo, number, snapshot),
    }
}

/// Bring the watches up to date. `refresh` polls only when the readings have
/// aged past it; `None` polls regardless.
fn poll(
    config: &Config,
    store: &inkling::adapters::watches_json::WatchStore,
    clock: &SystemClock,
    refresh: Option<TimeDelta>,
    why: &str,
) -> Result<inkling::watch::Polled, String> {
    let token = Secret::resolve(
        config.github.token_env.as_deref(),
        config.github.token_command.as_deref(),
    )
    .map_err(|e| e.to_string())?;

    let poller = inkling::watch::Poller {
        store,
        github: &GitHubProvider::new(&config.github.api_url, token),
        jira: &inkling::adapters::jira_acli::JiraProvider::new(),
        clock,
    };
    match refresh {
        Some(refresh) => poller.poll_if_stale(refresh, why),
        None => poller.poll(why),
    }
    .map_err(|e| e.to_string())
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
    }
}

/// Say so when something was over before the watch began.
///
/// A finished reference has its watch dropped by the first reading, so it
/// never appears in the list — and "watching nothing" is a poor answer to
/// "watch this pull request".
fn finished_already(
    polled: &inkling::watch::Polled,
    watches: &inkling::adapters::watches_json::Watches,
) {
    for (reference, sighting) in &polled.sightings {
        if watches.watches.contains_key(&reference.to_string()) {
            continue;
        }
        println!(
            "{reference}: already {} — nothing left to watch",
            standing(Some(&sighting.standing))
        );
    }
}

fn watch_out(
    watches: &inkling::adapters::watches_json::Watches,
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

    for (name, state, left, title) in &rows {
        let title = ellipsize(title, budget);
        println!("  {name:<name_width$}  {state:<standing_width$}  {left:<8}  {title}");
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
                    "at": event.at,
                    "reference": event.reference.to_string(),
                    "axis": event.axis.as_str(),
                    "from": event.from,
                    "to": event.to,
                    "title": event.title,
                    "detail": event.detail,
                })
            })
            .collect();
        // No cursor field: the last event's `seq` is the cursor, so there is
        // nothing to keep in sync with the rows.
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
    use super::{ellipsize, watch_target, watch_targets};
    use inkling::adapters::store_json::Snapshot;

    fn hosts() -> Vec<String> {
        vec!["github.com".to_string()]
    }

    #[test]
    fn owner_and_repo_name_a_pull_request_completely() {
        // Found by dogfooding: refusing this sent you to look up a host you
        // already knew. The fill-in-from-tracked rule is `run`'s, and `run`
        // needs it because a short form there selects among tracked records.
        let reference = watch_target("dpep/inkling#3", &hosts(), &Snapshot::default()).unwrap();
        assert_eq!(reference.to_string(), "github.com/dpep/inkling#3");
    }

    #[test]
    fn a_shorter_form_still_needs_something_tracked_to_match() {
        // `inkling#3` leaves out the owner, and guessing one would watch
        // somebody else's pull request.
        for target in ["inkling#3", "3"] {
            let error = watch_target(target, &hosts(), &Snapshot::default()).unwrap_err();
            assert!(error.contains("nothing tracked"), "{target}: {error}");
        }
    }

    #[test]
    fn one_unresolvable_reference_does_not_stop_the_others() {
        // Found by dogfooding: a typo in the second argument cost the watch
        // on the first one, which was the whole reason for running it.
        let targets = ["dpep/inkling#3".to_string(), "42".to_string()];
        let (references, refused) = watch_targets(&targets, &hosts(), &Snapshot::default());

        assert_eq!(references.len(), 1);
        assert_eq!(references[0].to_string(), "github.com/dpep/inkling#3");
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert!(refused[0].contains("#42"), "{refused:?}");
    }

    #[test]
    fn a_channel_is_refused_rather_than_watched() {
        let error = watch_target("#eng", &hosts(), &Snapshot::default()).unwrap_err();
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
