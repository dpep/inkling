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
    fn the_log_directory_sits_beside_the_state_file() {
        assert!(default_log_directory().ends_with("inkling/logs"));
    }
}
