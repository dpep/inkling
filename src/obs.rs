//! Logging, sized for a thing that runs unattended from cron.
//!
//! Two audiences. A human reading a terminal wants one readable line per run.
//! A human debugging a run that went wrong three days ago wants the file, with
//! enough structure to answer "which message, which PR, what state, why
//! nothing happened".
//!
//! Log files rotate daily and are capped, so an unattended job can't fill a
//! disk. Nothing here ever logs a token, and message text only appears at
//! trace level — an annotation tool shouldn't quietly build a copy of a
//! private Slack channel on disk.

use std::path::{Path, PathBuf};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Text,
    Json,
}

pub struct Options {
    pub format: Format,
    /// Where daily log files go. `None` means terminal only.
    pub directory: Option<PathBuf>,
    /// How many daily files to keep. Older ones are deleted.
    pub keep_days: usize,
    /// -v, -vv: raises the level for both sinks.
    pub verbosity: u8,
    /// Suppress the terminal sink; the file still gets everything.
    pub quiet: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            format: Format::Text,
            directory: None,
            keep_days: 14,
            verbosity: 0,
            quiet: false,
        }
    }
}

/// Holds the background writer open. Dropping it flushes; losing it early
/// means losing the tail of the log, which is exactly the part that explains
/// a crash.
pub struct Logging {
    _guard: Option<WorkerGuard>,
}

pub fn init(options: &Options) -> Logging {
    let level = match options.verbosity {
        0 => "inkling=info",
        1 => "inkling=debug",
        _ => "inkling=trace",
    };
    // RUST_LOG still wins, for the case where you need one noisy module.
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));

    let terminal = (!options.quiet).then(|| {
        fmt::layer()
            .with_target(false)
            .without_time()
            .with_writer(std::io::stderr)
            .with_filter(filter())
    });

    let mut guard = None;
    let file = options.directory.as_ref().and_then(|dir| {
        if let Err(error) = std::fs::create_dir_all(dir) {
            eprintln!("inkling: cannot write logs to {}: {error}", dir.display());
            return None;
        }
        let appender = RollingFileAppender::builder()
            .rotation(Rotation::DAILY)
            .filename_prefix("inkling")
            .filename_suffix("log")
            // Rotation alone still grows forever; this is what bounds it.
            .max_log_files(options.keep_days)
            .build(dir)
            .map_err(|e| eprintln!("inkling: cannot open log file: {e}"))
            .ok()?;

        let (writer, worker) = tracing_appender::non_blocking(appender);
        guard = Some(worker);

        // The file always gets full structure regardless of the terminal
        // format — it exists to be read by a debugger, not a person skimming.
        Some(match options.format {
            Format::Json => fmt::layer()
                .json()
                .with_writer(writer)
                .with_filter(filter())
                .boxed(),
            Format::Text => fmt::layer()
                .with_ansi(false)
                .with_writer(writer)
                .with_filter(filter())
                .boxed(),
        })
    });

    tracing_subscriber::registry()
        .with(terminal)
        .with(file)
        .init();
    Logging { _guard: guard }
}

/// `max_log_files` only prunes when it rotates, so a job that stops running
/// leaves its files forever. Sweeping on startup covers that.
pub fn prune(directory: &Path, keep_days: usize) -> std::io::Result<usize> {
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(
            60 * 60 * 24 * keep_days as u64,
        ))
        .unwrap_or(std::time::UNIX_EPOCH);

    let mut removed = 0;
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        // Only ever our own files — the directory may not be exclusively ours.
        let ours = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("inkling") && n.contains("log"));
        if !ours {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|modified| modified < cutoff);
        if stale && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Daily log files, oldest first. The rotation suffix is a date, so name
/// order is chronological order.
pub fn log_files(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("inkling") && n.contains("log"))
        })
        .collect();
    files.sort();
    files
}

/// The last `lines` log lines, optionally only those containing `needle`.
///
/// Reads newest file backwards until it has enough, so a long history costs
/// no more than a short one.
pub fn tail(directory: &Path, lines: usize, needle: Option<&str>) -> Vec<String> {
    let needle = needle.map(str::to_lowercase);
    let matches = |line: &String| match &needle {
        Some(n) => line.to_lowercase().contains(n),
        None => true,
    };

    let mut collected: Vec<String> = Vec::new();
    for file in log_files(directory).into_iter().rev() {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let mut found: Vec<String> = chronological(&text).into_iter().filter(matches).collect();
        found.append(&mut collected);
        collected = found;
        if collected.len() >= lines {
            break;
        }
    }
    let start = collected.len().saturating_sub(lines);
    collected.split_off(start)
}

/// One file's lines in the order they happened, rather than the order they
/// were appended.
///
/// A resident server writes to the same daily file as whatever the terminal
/// is running, so append order stopped being time order. Each line already
/// carries the answer; a line without one is a continuation and stays under
/// the line above it.
fn chronological(text: &str) -> Vec<String> {
    let mut keyed: Vec<(&str, &str)> = Vec::new();
    for line in text.lines() {
        let key = stamp(line).unwrap_or_else(|| keyed.last().map_or("", |(key, _)| *key));
        keyed.push((key, line));
    }
    // Stable, so lines sharing a timestamp keep the order they were written
    // in — which for one process is the order they happened in.
    keyed.sort_by_key(|(key, _)| *key);
    keyed
        .into_iter()
        .map(|(_, line)| line.to_string())
        .collect()
}

/// The RFC 3339 stamp `tracing` writes at the head of a text-format line.
///
/// A shape check rather than a parse: sorting only needs the field to be
/// comparable, and RFC 3339 in UTC compares correctly as text. JSON-format
/// lines carry theirs inside the object and so have none here, which leaves
/// them in file order.
fn stamp(line: &str) -> Option<&str> {
    let candidate = line.split_whitespace().next()?;
    let bytes = candidate.as_bytes();
    (bytes.len() >= 20 && bytes[4] == b'-' && bytes[10] == b'T' && bytes[bytes.len() - 1] == b'Z')
        .then_some(candidate)
}

/// Log lines stamped at or after `from`, an RFC 3339 prefix.
///
/// Only the two newest files are opened: the suffix is a date, so a 24-hour
/// window never spans more than two of them, and a window is not a reason to
/// read a fortnight.
pub fn since(directory: &Path, from: &str) -> Vec<String> {
    let mut files = log_files(directory);
    let recent = files.split_off(files.len().saturating_sub(2));
    recent
        .iter()
        .filter_map(|file| std::fs::read_to_string(file).ok())
        .flat_map(|text| {
            chronological(&text)
                .into_iter()
                .filter(|line| stamp(line).is_none_or(|at| at >= from))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// What the log says happened in a window.
///
/// The counts live in the log rather than the store because they are spread
/// across processes — a hook, a resident server, somebody at a terminal — and
/// the daily file is the only place those meet. Counted by the message each
/// line carries, so renaming a message means updating a marker here; the tests
/// pin the pairs against real lines.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Tally {
    pub subscribed: usize,
    pub watches: usize,
    pub transitions: usize,
    pub pushed: usize,
    pub withheld: usize,
    pub pulls: usize,
    pub hook_failures: usize,
    pub unread: usize,
    pub errors: usize,
    /// Upstream reads, from `polled watches`: what the reading schedule costs.
    pub github_queries: usize,
    pub pull_requests_read: usize,
    pub issues_read: usize,
}

/// The number after ` name=` on a line, or 0.
fn count(line: &str, name: &str) -> usize {
    line.split_once(&format!(" {name}="))
        .and_then(|(_, rest)| rest.split(' ').next()?.parse().ok())
        .unwrap_or(0)
}

impl Tally {
    pub fn of<'a>(lines: impl IntoIterator<Item = &'a str>) -> Self {
        let mut tally = Self::default();
        for line in lines {
            let counts = [
                (&mut tally.subscribed, line.contains(": subscribed ")),
                (&mut tally.watches, line.contains("watch registered")),
                (
                    &mut tally.transitions,
                    line.contains("transition reference="),
                ),
                (
                    &mut tally.pushed,
                    line.contains("pushed subscriber=") && line.contains(r#"outcome="written""#),
                ),
                (&mut tally.withheld, line.contains("withheld; the rest")),
                (&mut tally.pulls, line.contains("events read subscriber=")),
                (&mut tally.hook_failures, line.contains("hook failed")),
                (&mut tally.unread, line.contains("could not read watch")),
                (&mut tally.errors, line.contains(" ERROR ")),
            ];
            for (count, matched) in counts {
                *count += usize::from(matched);
            }
            if line.contains("polled watches ") {
                let pulls = count(line, "github");
                tally.github_queries += usize::from(pulls > 0);
                tally.pull_requests_read += pulls;
                tally.issues_read += count(line, "jira");
            }
        }
        tally
    }
}

pub fn default_log_directory() -> PathBuf {
    crate::adapters::store_json::JsonStore::default_path()
        .parent()
        .map(|p| p.join("logs"))
        .unwrap_or_else(|| std::env::temp_dir().join("inkling-logs"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("inkling-logs-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_aged(dir: &Path, name: &str, days_old: u64) {
        let path = dir.join(name);
        std::fs::write(&path, "log line\n").unwrap();
        let when = SystemTime::now() - Duration::from_secs(60 * 60 * 24 * days_old);
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(when).unwrap();
    }

    #[test]
    fn old_logs_are_deleted_and_recent_ones_kept() {
        let dir = TempDir::new("prune");
        write_aged(&dir.0, "inkling.log.2026-07-01", 30);
        write_aged(&dir.0, "inkling.log.2026-08-11", 1);

        assert_eq!(prune(&dir.0, 14).unwrap(), 1);
        assert!(!dir.0.join("inkling.log.2026-07-01").exists());
        assert!(dir.0.join("inkling.log.2026-08-11").exists());
    }

    #[test]
    fn pruning_leaves_other_peoples_files_alone() {
        // The state directory isn't exclusively ours.
        let dir = TempDir::new("foreign");
        write_aged(&dir.0, "something-else.log", 90);
        write_aged(&dir.0, "notes.txt", 90);

        assert_eq!(prune(&dir.0, 14).unwrap(), 0);
        assert!(dir.0.join("something-else.log").exists());
        assert!(dir.0.join("notes.txt").exists());
    }

    #[test]
    fn pruning_a_missing_directory_is_not_an_error() {
        assert_eq!(
            prune(Path::new("/nonexistent/inkling/logs"), 14).unwrap(),
            0
        );
    }

    #[test]
    fn tailing_returns_the_most_recent_lines_across_files() {
        let dir = TempDir::new("tail");
        std::fs::write(dir.0.join("inkling.2026-08-15.log"), "one\ntwo\n").unwrap();
        std::fs::write(dir.0.join("inkling.2026-08-16.log"), "three\nfour\n").unwrap();

        assert_eq!(tail(&dir.0, 3, None), ["two", "three", "four"]);
        // Fewer lines than asked for is not an error.
        assert_eq!(tail(&dir.0, 99, None).len(), 4);
    }

    #[test]
    fn tailing_can_filter_and_ignores_case() {
        let dir = TempDir::new("grep");
        std::fs::write(
            dir.0.join("inkling.2026-08-16.log"),
            "run starting\nERROR could not react\nrun finished\n",
        )
        .unwrap();

        assert_eq!(tail(&dir.0, 10, Some("error")), ["ERROR could not react"]);
        assert!(tail(&dir.0, 10, Some("nothing here")).is_empty());
    }

    #[test]
    fn interleaved_writers_are_read_back_in_time_order() {
        // Two processes appending to one daily file: the daemon's line landed
        // after the CLI's but happened before it.
        let dir = TempDir::new("interleaved");
        std::fs::write(
            dir.0.join("inkling.2026-09-20.log"),
            "2026-09-20T12:00:02.000100Z  INFO second\n\
             2026-09-20T12:00:01.000200Z  INFO first\n",
        )
        .unwrap();

        let lines = tail(&dir.0, 10, None);
        assert!(lines[0].ends_with("first"), "{lines:?}");
        assert!(lines[1].ends_with("second"), "{lines:?}");
    }

    #[test]
    fn a_line_without_a_timestamp_stays_under_the_one_above_it() {
        // Multi-line events, and anything else that isn't a log line: moving
        // them to the front would detach them from what they belong to.
        let dir = TempDir::new("continuation");
        std::fs::write(
            dir.0.join("inkling.2026-09-20.log"),
            "2026-09-20T12:00:02.000000Z  WARN boom\n\
             \t at the second line of a backtrace\n\
             2026-09-20T12:00:01.000000Z  INFO earlier\n",
        )
        .unwrap();

        assert_eq!(
            tail(&dir.0, 10, None)
                .iter()
                .map(|l| l.split_whitespace().last().unwrap().to_string())
                .collect::<Vec<_>>(),
            ["earlier", "boom", "backtrace"]
        );
    }

    #[test]
    fn tailing_a_missing_directory_is_empty_rather_than_an_error() {
        assert!(tail(Path::new("/nonexistent/inkling/logs"), 10, None).is_empty());
    }

    #[test]
    fn only_our_own_log_files_are_read() {
        let dir = TempDir::new("foreign-tail");
        std::fs::write(dir.0.join("inkling.2026-08-16.log"), "ours\n").unwrap();
        std::fs::write(dir.0.join("someone-else.log"), "theirs\n").unwrap();

        assert_eq!(tail(&dir.0, 10, None), ["ours"]);
    }

    /// Real lines, copied from a run, so a renamed message fails here.
    const SAMPLE: &str = "\
2026-09-21T00:38:01.505Z  INFO inkling::serve: serving pid=12705 lock=/x/serve.lock
2026-09-21T00:38:01.508Z  INFO inkling: subscribed subscriber=\"sess-a\" socket=/tmp/a.sock reattached=false
2026-09-21T00:38:01.509Z  INFO inkling: unsubscribed subscriber=\"sess-b\" detached=true
2026-09-21T00:38:02.000Z  INFO inkling: watch registered automatically reference=x#9 owner=\"s\" tool=\"Bash\" matched=\"gh pr view\"
2026-09-21T00:38:03.000Z  INFO inkling::watch: polled watches why=\"serve\" watched=3 github=2 jira=1 read=3 events=1 failures=0 elapsed_ms=412
2026-09-21T00:38:03.100Z  INFO inkling::watch: polled watches why=\"prompt\" watched=1 github=0 jira=1 read=1 events=0 failures=0 elapsed_ms=900
2026-09-21T00:38:03.515Z  INFO inkling::adapters::watches_json: transition reference=x#9 axis=\"state\" from=\"open\" to=\"approved\" seq=1 audience=1
2026-09-21T00:38:03.516Z  INFO inkling::push: pushed subscriber=\"sess-a\" seq=1 outcome=\"written\" elapsed_ms=0 advanced=true
2026-09-21T00:38:05.000Z  INFO inkling::push: pushed subscriber=\"sess-a\" seq=2 outcome=\"gone\" elapsed_ms=0 advanced=false
2026-09-21T00:38:06.000Z  INFO inkling::push: withheld; the rest waits for a prompt subscriber=\"sess-a\" owed=3 allowance=0
2026-09-21T00:38:07.000Z  INFO inkling::adapters::watches_json: events read subscriber=\"sess-a\" cursor_before=6 cursor_after=7 returned=1
2026-09-21T00:38:08.000Z  WARN inkling: hook failed event=\"prompt\" error=\"the store was busy for 250ms\"
2026-09-21T00:38:09.000Z  WARN inkling::watch: could not read watch reference=x#4 error=not found or not visible
2026-09-21T00:38:10.000Z ERROR inkling: inkling failed error=\"no token\"
";

    #[test]
    fn the_log_answers_what_happened_and_what_went_wrong() {
        assert_eq!(
            Tally::of(SAMPLE.lines()),
            Tally {
                subscribed: 1,
                watches: 1,
                transitions: 1,
                // The one that went nowhere doesn't count as delivered.
                pushed: 1,
                withheld: 1,
                pulls: 1,
                hook_failures: 1,
                unread: 1,
                errors: 1,
                // Pull requests go in one batched query; issues one apiece.
                github_queries: 1,
                pull_requests_read: 2,
                issues_read: 2,
            }
        );
    }

    #[test]
    fn a_window_reads_only_the_lines_inside_it() {
        let dir = TempDir::new("since");
        std::fs::write(dir.0.join("inkling.2026-09-20.log"), SAMPLE).unwrap();
        std::fs::write(
            dir.0.join("inkling.2026-09-21.log"),
            "2026-09-21T00:39:00.000Z  INFO inkling: subscribed subscriber=\"late\"\n",
        )
        .unwrap();

        let recent = since(&dir.0, "2026-09-21T00:38:09");
        assert_eq!(Tally::of(recent.iter().map(String::as_str)).subscribed, 1);
        assert_eq!(Tally::of(recent.iter().map(String::as_str)).errors, 1);
        assert_eq!(recent.len(), 3, "{recent:?}");
    }

    #[test]
    fn the_log_directory_sits_beside_the_state_file() {
        assert!(default_log_directory().ends_with("inkling/logs"));
    }
}
