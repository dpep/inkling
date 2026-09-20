//! One resident inkling per machine: started by whoever needs it, gone when
//! nobody does.
//!
//! docs/waiting.md rejected a daemon and those objections still stand, so this
//! answers them rather than ignoring them. Nobody starts it by hand —
//! `ensure_running` spawns one when no process holds the lock. It owns no
//! state, so a `kill -9` loses nothing and the next caller respawns it. It
//! holds no claude/MCP bridge, so "boots broken, stays broken" doesn't apply.
//! And it exits the moment it has no reason to live. It is waits.json's poller
//! lease made resident.
//!
//! What a tick *does* is somebody else's business. This module owns the lock,
//! the clock, the exit conditions, and the log — nothing else.

use crate::adapters::lock::{FileLock, LockError};
use crate::ports::{Clock, RealSleeper, Sleeper, SystemClock};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// A failing tick is usually a failing dependency; capped low so recovery is
/// still prompt once it comes back.
const MAX_BACKOFF_FACTOR: u32 = 8;

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error("spawning a server: {0}")]
    Spawn(#[from] std::io::Error),
}

/// What one tick found, and the only thing that keeps the server alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pulse {
    /// Somebody is being served.
    Busy,
    /// Nothing to do. Long enough of these and the server stands down.
    Idle,
}

/// What a tick reports back. An error is transient as far as the loop is
/// concerned: it logs, backs off, and comes round again.
pub type Ticked = Result<Pulse, Box<dyn std::error::Error>>;

/// The injected half of a server: do one tick, and say whether that tick found
/// a reason to still be here.
///
/// One method rather than two, deliberately. "Is there work" is answered by
/// the work itself, so a tick and a separate liveness probe can't disagree,
/// and the loop reads nothing the tick hasn't already read.
pub trait Work {
    fn tick(&self) -> Ticked;
}

impl<F: Fn() -> Ticked> Work for F {
    fn tick(&self) -> Ticked {
        self()
    }
}

/// Whether someone has asked this process to stop. Behind a trait for the same
/// reason `Clock` is: a test shouldn't have to signal itself to assert the
/// shutdown path.
pub trait Shutdown {
    fn requested(&self) -> bool;
}

static SIGNALLED: AtomicBool = AtomicBool::new(false);

extern "C" fn note_signal(_: libc::c_int) {
    SIGNALLED.store(true, Ordering::Relaxed);
}

/// SIGTERM and SIGINT, noted in a flag the loop reads between ticks.
pub struct Signals;

impl Signals {
    /// Installs process-wide handlers, once. Setting an atomic is all a
    /// handler may safely do, which is why the loop does the reacting.
    pub fn install() -> Self {
        static ONCE: std::sync::Once = std::sync::Once::new();
        // Handing a fn pointer to the kernel; the handler itself is
        // async-signal-safe.
        ONCE.call_once(|| {
            let handler = note_signal as *const () as libc::sighandler_t;
            unsafe {
                libc::signal(libc::SIGTERM, handler);
                libc::signal(libc::SIGINT, handler);
            }
        });
        Self
    }
}

impl Shutdown for Signals {
    fn requested(&self) -> bool {
        SIGNALLED.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// How often to tick.
    pub interval: Duration,
    /// How long with nothing to do before standing down. Generous on purpose:
    /// a session that detaches and reattaches shouldn't cost a respawn.
    pub idle_after: Duration,
    /// A cap, so a resident process eventually picks up a newer build. It
    /// stands down; the next caller respawns whatever is installed then.
    pub max_lifetime: Duration,
    /// Held for the whole run. The kernel releases it on death, which is what
    /// makes "is a server running" a question anyone can answer.
    pub lock: PathBuf,
    /// Display only, and disposable — rewritten every tick.
    pub status: PathBuf,
}

impl Config {
    pub fn in_dir(dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref();
        Self {
            interval: Duration::from_secs(15),
            idle_after: Duration::from_secs(300),
            max_lifetime: Duration::from_secs(60 * 60 * 24),
            lock: dir.join("serve.lock"),
            status: dir.join("serve.json"),
        }
    }

    /// Beside the run state, like every other file inkling keeps.
    pub fn default_dir() -> PathBuf {
        crate::adapters::store_json::JsonStore::default_path()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::in_dir(Self::default_dir())
    }
}

/// Why a server stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Nothing to do for long enough. The ordinary ending.
    Idle,
    MaxLifetime,
    Signalled,
}

impl Exit {
    pub fn as_str(self) -> &'static str {
        match self {
            Exit::Idle => "idle",
            Exit::MaxLifetime => "max_lifetime",
            Exit::Signalled => "signalled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub exit: Exit,
    pub uptime: Duration,
    pub ticks: u32,
    /// Ticks that returned an error and were ridden out.
    pub failures: u32,
    pub longest_tick: Duration,
}

#[derive(Debug)]
pub enum Outcome {
    /// Somebody else holds the lock. The normal case for the second caller,
    /// not a fault.
    AlreadyRunning,
    Served(Report),
}

/// What the running server last wrote about itself.
///
/// No version field: this is display data, rebuilt every tick and safe to
/// delete. A format it can't parse reads as "no details", not as an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub last_tick_at: Option<DateTime<Utc>>,
    pub ticks: u32,
    pub failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug)]
pub struct Status {
    /// From the lock, never from the file.
    pub running: bool,
    /// The last thing a server wrote. Present whether or not one is running —
    /// when it isn't, this says when the last one was here.
    pub record: Option<Record>,
}

/// Is a server running, and what does it say about itself?
///
/// The verdict comes from the lock, because that is the only claim the kernel
/// stands behind: a `kill -9` leaves the status file exactly as it was, so
/// trusting the file would report a corpse as healthy.
pub fn read_status(config: &Config) -> Status {
    let running = matches!(FileLock::try_acquire(&config.lock), Ok(None));
    Status {
        running,
        record: read_record(&config.status),
    }
}

fn read_record(path: &Path) -> Option<Record> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

#[derive(Debug, PartialEq, Eq)]
pub enum Ensured {
    AlreadyRunning,
    Spawned { pid: u32 },
}

/// Make sure a server exists, and return without waiting for it.
///
/// Two callers at once both find the lock free and both spawn; the flock in
/// `Serve::run` picks the winner and the loser stands down quietly. That race
/// is the arbitration, not a case to guard against.
pub fn ensure_running(config: &Config, args: &[&str], reason: &str) -> Result<Ensured, ServeError> {
    match FileLock::try_acquire(&config.lock)? {
        None => Ok(Ensured::AlreadyRunning),
        Some(lock) => {
            // Released before spawning, or the child we just started would be
            // locked out by its own parent.
            drop(lock);
            let pid = spawn_detached(&std::env::current_exe()?, args)?;
            tracing::info!(pid, %reason, "spawned a server");
            Ok(Ensured::Spawned { pid })
        }
    }
}

/// A new session with no controlling terminal, stdio discarded, cwd at the
/// root — so closing the terminal that spawned it, or deleting the directory
/// it was spawned from, can't take it down.
fn spawn_detached(exe: &Path, args: &[&str]) -> std::io::Result<u32> {
    let mut command = Command::new(exe);
    command
        .args(args)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // Between fork and exec: `setsid` is async-signal-safe, which is the bar
    // for anything run here.
    unsafe {
        command.pre_exec(|| match libc::setsid() {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        });
    }

    // Not waited on: the child reparents to init when this process exits,
    // which for a CLI caller is moments later.
    Ok(command.spawn()?.id())
}

/// The loop. Its seams are `Work` and the clock, so a day-long lifetime is
/// asserted in microseconds.
pub struct Serve<'a> {
    pub config: Config,
    pub work: &'a dyn Work,
    pub clock: &'a dyn Clock,
    pub sleeper: &'a dyn Sleeper,
    pub shutdown: &'a dyn Shutdown,
}

impl Serve<'_> {
    pub fn run(&self) -> Result<Outcome, ServeError> {
        let Some(_lock) = FileLock::try_acquire(&self.config.lock)? else {
            tracing::info!(
                lock = %self.config.lock.display(),
                "another inkling is already serving; standing down"
            );
            return Ok(Outcome::AlreadyRunning);
        };

        let started = self.clock.now();
        let mut record = Record {
            pid: std::process::id(),
            started_at: started,
            last_tick_at: None,
            ticks: 0,
            failures: 0,
            last_error: None,
        };
        tracing::info!(
            pid = record.pid,
            lock = %self.config.lock.display(),
            interval_seconds = self.config.interval.as_secs(),
            idle_after_seconds = self.config.idle_after.as_secs(),
            max_lifetime_seconds = self.config.max_lifetime.as_secs(),
            "serving"
        );

        // When the current run of nothing-to-do began. `None` means the last
        // tick found work.
        let mut idle_since = None;
        let mut consecutive_failures = 0u32;
        let mut longest_tick = Duration::ZERO;

        let exit = loop {
            let tick_started = self.clock.now();
            let outcome = self.work.tick();
            let now = self.clock.now();
            let tick_took = since(tick_started, now);
            longest_tick = longest_tick.max(tick_took);
            record.ticks += 1;
            record.last_tick_at = Some(now);

            match outcome {
                Ok(pulse) => {
                    consecutive_failures = 0;
                    record.last_error = None;
                    idle_since = match pulse {
                        Pulse::Busy => None,
                        Pulse::Idle => idle_since.or(Some(now)),
                    };
                    tracing::debug!(
                        tick = record.ticks,
                        elapsed_ms = tick_took.as_millis() as u64,
                        outcome = match pulse {
                            Pulse::Busy => "busy",
                            Pulse::Idle => "idle",
                        },
                        idle_seconds = idle_since.map_or(0, |start| since(start, now).as_secs()),
                        "tick"
                    );
                }
                Err(error) => {
                    consecutive_failures += 1;
                    record.failures += 1;
                    record.last_error = Some(error.to_string());
                    tracing::warn!(
                        %error,
                        consecutive = consecutive_failures,
                        elapsed_ms = tick_took.as_millis() as u64,
                        "tick failed; still serving"
                    );
                }
            }
            self.write_status(&record);

            if let Some(idle_since) = idle_since
                && since(idle_since, now) >= self.config.idle_after
            {
                break Exit::Idle;
            }
            if since(started, now) >= self.config.max_lifetime {
                break Exit::MaxLifetime;
            }
            if self.shutdown.requested() {
                break Exit::Signalled;
            }
            self.sleeper.sleep(self.backoff(consecutive_failures));
        };

        let report = Report {
            exit,
            uptime: since(started, self.clock.now()),
            ticks: record.ticks,
            failures: record.failures,
            longest_tick,
        };
        tracing::info!(
            reason = report.exit.as_str(),
            uptime_seconds = report.uptime.as_secs(),
            ticks = report.ticks,
            failures = report.failures,
            longest_tick_ms = report.longest_tick.as_millis() as u64,
            "serve finished"
        );
        Ok(Outcome::Served(report))
    }

    fn backoff(&self, consecutive_failures: u32) -> Duration {
        let factor = 2u32
            .saturating_pow(consecutive_failures)
            .min(MAX_BACKOFF_FACTOR);
        self.config.interval.saturating_mul(factor.max(1))
    }

    /// By rename, so a reader never sees half a record. A write that fails is
    /// worth saying out loud but is not worth stopping for — nothing depends
    /// on this file.
    fn write_status(&self, record: &Record) {
        if let Err(error) = atomically(&self.config.status, record) {
            tracing::warn!(%error, path = %self.config.status.display(), "cannot write the status file");
        }
    }
}

fn atomically(path: &Path, record: &Record) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, serde_json::to_string_pretty(record)?)?;
    std::fs::rename(&temp, path)
}

/// Wall-clock durations don't go backwards here; a clock that jumps reads as
/// zero rather than as an enormous number.
fn since(start: DateTime<Utc>, now: DateTime<Utc>) -> Duration {
    (now - start).to_std().unwrap_or(Duration::ZERO)
}

/// Run a server in this process, with the real clock, the real sleeper, and
/// real signals. `Serve` is the same loop with those swapped out.
pub fn run(config: Config, work: &dyn Work) -> Result<Outcome, ServeError> {
    Serve {
        config,
        work,
        clock: &SystemClock,
        sleeper: &RealSleeper,
        shutdown: &Signals::install(),
    }
    .run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FixedClock;
    use std::cell::Cell;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("inkling-serve-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn config(&self) -> Config {
            Config {
                interval: Duration::from_secs(1),
                idle_after: Duration::from_secs(10),
                max_lifetime: Duration::from_secs(3600),
                ..Config::in_dir(&self.0)
            }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Sleeping moves the clock rather than the wall.
    struct Fast<'a>(&'a FixedClock);

    impl Sleeper for Fast<'_> {
        fn sleep(&self, duration: Duration) {
            self.0
                .advance(chrono::TimeDelta::from_std(duration).unwrap());
        }
    }

    struct Never;

    impl Shutdown for Never {
        fn requested(&self) -> bool {
            false
        }
    }

    fn serve(config: Config, work: &dyn Work) -> Report {
        let clock = FixedClock::at("2026-09-20T12:00:00Z");
        let sleeper = Fast(&clock);
        let outcome = Serve {
            config,
            work,
            clock: &clock,
            sleeper: &sleeper,
            shutdown: &Never,
        }
        .run()
        .expect("served");
        match outcome {
            Outcome::Served(report) => report,
            Outcome::AlreadyRunning => panic!("nobody else held the lock"),
        }
    }

    #[test]
    fn a_server_with_nothing_to_do_stands_down() {
        let scratch = Scratch::new("idle");
        let report = serve(scratch.config(), &(|| Ok(Pulse::Idle)));

        assert_eq!(report.exit, Exit::Idle);
        // First tick starts the idle run at t=0; ticks at 1s..10s follow.
        assert_eq!(report.uptime, Duration::from_secs(10));
        assert_eq!(report.ticks, 11);
    }

    #[test]
    fn work_resets_the_idle_clock() {
        let scratch = Scratch::new("busy");
        // Busy for the first five ticks, then nothing.
        let ticks = Cell::new(0);
        let report = serve(
            scratch.config(),
            &(|| {
                ticks.set(ticks.get() + 1);
                match ticks.get() <= 5 {
                    true => Ok(Pulse::Busy),
                    false => Ok(Pulse::Idle),
                }
            }),
        );

        assert_eq!(report.exit, Exit::Idle);
        assert_eq!(
            report.uptime,
            Duration::from_secs(15),
            "five seconds of work, then the full idle window"
        );
    }

    #[test]
    fn it_stands_down_at_its_lifetime_cap() {
        let scratch = Scratch::new("lifetime");
        let config = Config {
            max_lifetime: Duration::from_secs(60),
            ..scratch.config()
        };
        let report = serve(config, &(|| Ok(Pulse::Busy)));

        assert_eq!(report.exit, Exit::MaxLifetime);
        assert_eq!(report.uptime, Duration::from_secs(60));
    }

    #[test]
    fn a_failing_tick_does_not_end_the_run() {
        let scratch = Scratch::new("failing");
        let ticks = Cell::new(0);
        let report = serve(
            scratch.config(),
            &(|| {
                ticks.set(ticks.get() + 1);
                match ticks.get() {
                    1..=3 => Err("github is having a moment".into()),
                    _ => Ok(Pulse::Idle),
                }
            }),
        );

        assert_eq!(report.exit, Exit::Idle, "it outlived the failures");
        assert_eq!(report.failures, 3);
    }

    #[test]
    fn repeated_failures_back_off_rather_than_spin() {
        let scratch = Scratch::new("backoff");
        let config = Config {
            max_lifetime: Duration::from_secs(60),
            ..scratch.config()
        };
        let report = serve(config, &(|| Err("still broken".into())));

        assert_eq!(report.exit, Exit::MaxLifetime);
        // Naps of 2s, 4s, 8s, then capped at 8s: 60s of failing costs far
        // fewer ticks than the 1s interval would.
        assert_eq!(report.ticks, 10);
        assert_eq!(report.failures, 10);
    }

    #[test]
    fn a_failure_is_forgotten_once_a_tick_succeeds() {
        let scratch = Scratch::new("recovered");
        let ticks = Cell::new(0);
        let config = scratch.config();
        let status = config.status.clone();
        serve(
            config,
            &(|| {
                ticks.set(ticks.get() + 1);
                match ticks.get() {
                    1 => Err("blip".into()),
                    _ => Ok(Pulse::Idle),
                }
            }),
        );

        let record = read_record(&status).expect("a record was written");
        assert_eq!(record.failures, 1, "the count is the lasting evidence");
        assert_eq!(record.last_error, None, "the message is not");
    }

    #[test]
    fn a_term_signal_ends_the_run() {
        // The real handler, raised at ourselves. No other test installs
        // `Signals`, so the process-wide flag can't leak into one.
        let scratch = Scratch::new("signal");
        let clock = FixedClock::at("2026-09-20T12:00:00Z");
        let sleeper = Fast(&clock);
        let signals = Signals::install();
        let work = || {
            unsafe { libc::raise(libc::SIGTERM) };
            Ok(Pulse::Busy)
        };
        let outcome = Serve {
            config: scratch.config(),
            work: &work,
            clock: &clock,
            sleeper: &sleeper,
            shutdown: &signals,
        }
        .run()
        .expect("served");

        match outcome {
            Outcome::Served(report) => {
                assert_eq!(report.exit, Exit::Signalled);
                assert_eq!(report.ticks, 1, "it finished the tick it was in");
            }
            Outcome::AlreadyRunning => panic!("nobody else held the lock"),
        }
    }

    #[test]
    fn a_second_server_stands_down_rather_than_failing() {
        let scratch = Scratch::new("contended");
        let config = scratch.config();
        let held = FileLock::try_acquire(&config.lock).unwrap();
        assert!(held.is_some());

        let clock = FixedClock::at("2026-09-20T12:00:00Z");
        let sleeper = Fast(&clock);
        let work = || Ok(Pulse::Idle);
        let outcome = Serve {
            config,
            work: &work,
            clock: &clock,
            sleeper: &sleeper,
            shutdown: &Never,
        }
        .run()
        .expect("standing down is not an error");

        assert!(matches!(outcome, Outcome::AlreadyRunning));
    }

    #[test]
    fn status_comes_from_the_lock_not_the_file() {
        // What a `kill -9` leaves behind: a plausible record and no holder.
        let scratch = Scratch::new("stale");
        let config = scratch.config();
        atomically(
            &config.status,
            &Record {
                pid: 999_999,
                started_at: "2026-09-20T12:00:00Z".parse().unwrap(),
                last_tick_at: None,
                ticks: 7,
                failures: 0,
                last_error: None,
            },
        )
        .unwrap();

        let status = read_status(&config);
        assert!(!status.running, "a corpse must not read as healthy");
        assert_eq!(status.record.map(|r| r.ticks), Some(7));
    }

    #[test]
    fn status_reports_running_while_the_lock_is_held() {
        let scratch = Scratch::new("live");
        let config = scratch.config();
        let _held = FileLock::try_acquire(&config.lock).unwrap().unwrap();

        assert!(read_status(&config).running);
    }

    #[test]
    fn status_without_a_file_is_not_an_error() {
        let scratch = Scratch::new("bare");
        let status = read_status(&scratch.config());
        assert!(!status.running);
        assert!(status.record.is_none());
    }

    #[test]
    fn a_running_server_needs_no_spawning() {
        let scratch = Scratch::new("ensure");
        let config = scratch.config();
        let _held = FileLock::try_acquire(&config.lock).unwrap().unwrap();

        assert_eq!(
            ensure_running(&config, &["serve"], "test").unwrap(),
            Ensured::AlreadyRunning
        );
    }

    #[test]
    fn the_running_server_records_itself() {
        let scratch = Scratch::new("record");
        let config = scratch.config();
        let status = config.status.clone();
        serve(config, &(|| Ok(Pulse::Idle)));

        let record = read_record(&status).expect("a record was written");
        assert_eq!(record.pid, std::process::id());
        assert_eq!(record.ticks, 11);
        assert!(record.last_tick_at.is_some());
    }
}
