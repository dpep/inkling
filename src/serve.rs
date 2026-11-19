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
//! the clock, the exit conditions, and the log — nothing else. Work too slow
//! for a tick runs `beside` the loop, on a thread that lives no longer than
//! the lock does.

use crate::adapters::lock::{FileLock, LockError};
use crate::ports::{Clock, RealSleeper, Sleeper, SystemClock};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
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
    /// The state on disk belongs to a newer build, so this one must not
    /// touch it. Stands down at once.
    Superseded,
}

/// What a successful tick found, and when it next wants to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Beat {
    pub pulse: Pulse,
    /// `None` is no opinion: come back after the interval.
    pub next: Option<DateTime<Utc>>,
}

impl Pulse {
    pub fn as_str(self) -> &'static str {
        match self {
            Pulse::Busy => "busy",
            Pulse::Idle => "idle",
            Pulse::Superseded => "superseded",
        }
    }
}

impl From<Pulse> for Beat {
    fn from(pulse: Pulse) -> Self {
        Self { pulse, next: None }
    }
}

/// What a tick reports back. An error is transient as far as the loop is
/// concerned: it logs, backs off, and comes round again.
pub type Ticked = Result<Beat, Box<dyn std::error::Error>>;

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
pub trait Shutdown: Sync {
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
        signalled()
    }
}

/// Whether this process has been asked to stop since `Signals::install`.
pub fn signalled() -> bool {
    SIGNALLED.load(Ordering::Relaxed)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The shortest nap between ticks, and the nap when the work names no
    /// time of its own. Otherwise a server sleeps until its work is next due.
    pub interval: Duration,
    /// How long with nothing to do before standing down. Generous on purpose:
    /// a session that detaches and reattaches shouldn't cost a respawn.
    pub idle_after: Duration,
    /// A cap, so a resident process eventually picks up a newer build. With
    /// work left it hands off to whatever is installed now; see `hand_off`.
    pub max_lifetime: Duration,
    /// Held for the whole run. The kernel releases it on death, which is what
    /// makes "is a server running" a question anyone can answer.
    pub lock: PathBuf,
    /// Display only, and disposable — rewritten every tick.
    pub status: PathBuf,
    /// A FIFO that cuts a nap short: see `ring`.
    pub doorbell: PathBuf,
    /// The binary this server was started from. When a new build is renamed
    /// over it, the server stands down and hands off to the new one.
    pub executable: Option<PathBuf>,
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
            doorbell: dir.join("serve.wake"),
            executable: None,
        }
    }

    /// Beside `state.json`, like every other file inkling keeps.
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
    /// A new build was installed over the binary it is running.
    Replaced,
    /// A newer build wrote the store, but this one's executable is unchanged:
    /// a stale binary, or a downgrade. Handing off would start itself again.
    Superseded,
}

impl Exit {
    pub fn as_str(self) -> &'static str {
        match self {
            Exit::Idle => "idle",
            Exit::MaxLifetime => "max_lifetime",
            Exit::Signalled => "signalled",
            Exit::Replaced => "replaced",
            Exit::Superseded => "superseded",
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
    /// Whether the last tick still found somebody to serve.
    pub busy: bool,
}

impl Report {
    /// Whether to start a successor rather than leave it to the next hook.
    /// Only for the exits that are about the process, not the work: an idle
    /// session fires no hooks, so standing down with work left is a silence.
    pub fn hands_off(&self) -> bool {
        self.busy && matches!(self.exit, Exit::Replaced | Exit::MaxLifetime)
    }
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

impl Status {
    /// Serving and ticking: the last tick ended within two intervals. A
    /// server waits out a Slack read in flight before exiting, holding the
    /// lock without ticking.
    pub fn ticking(&self, now: DateTime<Utc>, interval: Duration) -> bool {
        let Some(record) = self.record.as_ref().filter(|_| self.running) else {
            return false;
        };
        let last = record.last_tick_at.unwrap_or(record.started_at);
        since(last, now) <= interval.saturating_mul(2)
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

/// Make sure a server exists, and don't return until it is one.
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
            if !claimed(&config.status, pid, TAKEOFF_LOOKS, &RealSleeper) {
                // Not a failure. Either the child is slower than the window,
                // or it lost the lock race and stood down — in which case the
                // winner is already serving.
                tracing::debug!(pid, "the spawned server has not claimed the lock yet");
            }
            Ok(Ensured::Spawned { pid })
        }
    }
}

/// How long to wait for a just-spawned server to claim the lock, as a number of
/// looks and the gap between them — 250ms in all.
///
/// So that `inkling status` a moment after the command that spawned one doesn't
/// report `serving no`. The child takes about 40ms to get there: 5ms to exec,
/// 27ms for `gh auth token`, the rest config and the lock. This is paid on the
/// spawn, which happens once per server rather than once per caller — every
/// other caller finds the lock held and returns immediately.
const TAKEOFF_LOOKS: u32 = 25;
const TAKEOFF_LOOK: Duration = Duration::from_millis(10);

/// 5s, for a successor. Nobody waits on a server that is leaving, and the first
/// exec of a just-installed binary costs about a second while macOS assesses it.
const HANDOFF_LOOKS: u32 = 500;

/// Whether the server at `pid` has claimed the lock within `looks`.
///
/// Reads the status file rather than probing the lock, because probing *takes*
/// the lock when it is free — which in this window would lock out the very
/// child being waited for. A server writes its record as its first act after
/// acquiring, so the record naming the pid is the arrival signal.
fn claimed(status: &Path, pid: u32, looks: u32, sleeper: &dyn Sleeper) -> bool {
    for _ in 0..looks {
        if read_record(status).is_some_and(|record| record.pid == pid) {
            return true;
        }
        sleeper.sleep(TAKEOFF_LOOK);
    }
    false
}

/// A new session with no controlling terminal, stdio discarded, cwd at the
/// root — so closing the terminal that spawned it, or deleting the directory
/// it was spawned from, can't take it down.
fn spawn_detached(exe: &Path, args: &[impl AsRef<OsStr>]) -> std::io::Result<u32> {
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
    /// Work too slow for a tick, run on its own thread for as long as this
    /// process holds the lock.
    pub beside: Option<&'a Worker<'a>>,
}

pub type Worker<'a> = dyn Fn(&Beside) + Sync + 'a;

/// What a worker beside the loop is handed: when to stop, and a way to wake
/// the loop to deliver what it recorded.
pub struct Beside<'a> {
    done: &'a AtomicBool,
    shutdown: &'a dyn Shutdown,
    /// Set each time the loop wakes, so a ring reaches the worker too: only
    /// the loop can read the doorbell.
    woken: &'a AtomicBool,
    doorbell: &'a Path,
}

impl Beside<'_> {
    /// The loop has finished, or a signal asked it to.
    pub fn stopping(&self) -> bool {
        self.done.load(Ordering::Relaxed) || self.shutdown.requested()
    }

    pub fn wake(&self) {
        ring(self.doorbell);
    }

    /// Up to `duration`, until the loop wakes, or until it is time to stop.
    /// A wake during the worker's round ends its next nap at once.
    pub fn nap(&self, duration: Duration) {
        let until = std::time::Instant::now() + duration;
        while !self.stopping() && !self.woken.swap(false, Ordering::Relaxed) {
            let left = until.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            std::thread::sleep(left.min(Duration::from_millis(100)));
        }
    }
}

/// Sets the flag however the loop ends, a panic included: the scope joins the
/// worker before unwinding, and a worker never told would hang it.
struct Finished<'a>(&'a AtomicBool);

impl Drop for Finished<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
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
        // Before the first tick, because taking the lock is the news and a
        // tick can take a second. It is also what the caller that spawned us
        // waits for — see `claimed`.
        self.write_status(&record);

        // When the current run of nothing-to-do began. `None` means the last
        // tick found work.
        let built = self.config.executable.as_deref().and_then(build_of);
        let mut idle_since = None;
        let mut consecutive_failures = 0u32;
        let mut longest_tick = Duration::ZERO;
        // When the work last said it next wants to run.
        let mut next = None;
        let mut superseded = false;

        let replaced = || match (built, &self.config.executable) {
            (Some(built), Some(path)) if build_of(path) != Some(built) => {
                tracing::info!(executable = %path.display(), "a new build was installed; standing down for it");
                true
            }
            _ => false,
        };

        let done = AtomicBool::new(false);
        let woken = AtomicBool::new(false);
        let exit = std::thread::scope(|scope| {
            if let Some(worker) = self.beside {
                let beside = Beside {
                    done: &done,
                    shutdown: self.shutdown,
                    woken: &woken,
                    doorbell: &self.config.doorbell,
                };
                scope.spawn(move || worker(&beside));
            }
            let _finished = Finished(&done);
            loop {
                // Before as well as after: a nap can span an install, and the new
                // build's hooks may already have written a store this one can't.
                if replaced() {
                    break Exit::Replaced;
                }
                let tick_started = self.clock.now();
                let outcome = self.work.tick();
                let now = self.clock.now();
                let tick_took = since(tick_started, now);
                longest_tick = longest_tick.max(tick_took);
                record.ticks += 1;
                record.last_tick_at = Some(now);

                match outcome {
                    Ok(Beat {
                        pulse,
                        next: wanted,
                    }) => {
                        consecutive_failures = 0;
                        record.last_error = None;
                        next = wanted;
                        idle_since = match pulse {
                            Pulse::Busy => None,
                            Pulse::Idle => idle_since.or(Some(now)),
                            Pulse::Superseded => {
                                superseded = true;
                                idle_since
                            }
                        };
                        tracing::debug!(
                            tick = record.ticks,
                            elapsed_ms = tick_took.as_millis() as u64,
                            outcome = pulse.as_str(),
                            idle_seconds =
                                idle_since.map_or(0, |start| since(start, now).as_secs()),
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
                if replaced() {
                    break Exit::Replaced;
                }
                if superseded {
                    tracing::info!("a newer build owns the store; standing down for it");
                    break Exit::Superseded;
                }
                if self.shutdown.requested() {
                    break Exit::Signalled;
                }
                let nap = match consecutive_failures {
                    0 => self.nap(now, next, started, idle_since),
                    failures => self.backoff(failures),
                };
                tracing::debug!(nap_seconds = nap.as_secs(), "napping");
                self.sleeper.sleep(nap);
                woken.store(true, Ordering::Relaxed);
            }
        });

        let report = Report {
            exit,
            uptime: since(started, self.clock.now()),
            ticks: record.ticks,
            failures: record.failures,
            longest_tick,
            busy: idle_since.is_none(),
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

    /// Until the work is next due, but no later than a deadline of the loop's
    /// own and never shorter than the interval — something that stays due,
    /// like a watch that can't be read, must not become a spin.
    fn nap(
        &self,
        now: DateTime<Utc>,
        next: Option<DateTime<Utc>>,
        started: DateTime<Utc>,
        idle_since: Option<DateTime<Utc>>,
    ) -> Duration {
        let Some(next) = next else {
            return self.config.interval;
        };
        let lifetime_left = self.config.max_lifetime.saturating_sub(since(started, now));
        let idle_left =
            idle_since.map(|start| self.config.idle_after.saturating_sub(since(start, now)));
        [Some(since(now, next)), Some(lifetime_left), idle_left]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(self.config.interval)
            .max(self.config.interval)
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

/// What identifies one build of the binary at `path`: a rename over it changes
/// the inode, a write in place the mtime. The path, not the running image —
/// macOS has no `/proc/self/exe` to name the old inode by.
type Build = (u64, u64, i64, i64);

fn build_of(path: &Path) -> Option<Build> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.dev(), meta.ino(), meta.mtime(), meta.mtime_nsec()))
}

/// Wall-clock durations don't go backwards here; a clock that jumps reads as
/// zero rather than as an enormous number.
fn since(start: DateTime<Utc>, now: DateTime<Utc>) -> Duration {
    (now - start).to_std().unwrap_or(Duration::ZERO)
}

/// A named pipe beside the lock that anyone may ring to end a server's nap.
///
/// A server sleeps until its next watch is due, which can be an hour; a watch
/// registered or touched meanwhile is due now. A pipe rather than a signal: a
/// signal needs a pid, and a stale pid can name a stranger, whose default
/// action on SIGUSR1 is to die.
pub struct Doorbell {
    pipe: Option<std::fs::File>,
    /// The longest nap when there is no pipe to be woken through.
    fallback: Duration,
}

impl Doorbell {
    pub fn install(path: &Path, fallback: Duration) -> Self {
        let pipe = match open_pipe(path) {
            Ok(pipe) => Some(pipe),
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "no doorbell; napping at most the interval");
                None
            }
        };
        Self { pipe, fallback }
    }
}

fn open_pipe(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};

    let name = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    if !std::fs::metadata(path)?.file_type().is_fifo() {
        return Err(std::io::Error::other("exists and is not a pipe"));
    }
    // Opened for writing too, so the server is always a writer of its own
    // pipe and `poll` never reports a hangup when a ringer closes.
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

impl Sleeper for Doorbell {
    fn sleep(&self, duration: Duration) {
        use std::io::Read;
        use std::os::fd::AsRawFd;

        let Some(pipe) = &self.pipe else {
            return std::thread::sleep(duration.min(self.fallback));
        };
        let mut ready = libc::pollfd {
            fd: pipe.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // Rounded up: waking just before a watch is due finds nothing due.
        let millis = duration.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
        if unsafe { libc::poll(&mut ready, 1, millis) } > 0 {
            // Several rings during one nap are one wake-up.
            let mut rung = [0u8; 64];
            while matches!((&*pipe).read(&mut rung), Ok(n) if n > 0) {}
        }
    }
}

/// Wake the server, if one is asleep. Never blocks and never fails: with no
/// server the pipe has no reader, and a non-blocking open refuses at once.
pub fn ring(path: &Path) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if let Ok(mut pipe) = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        let _ = pipe.write(&[1]);
    }
}

/// Run a server in this process, with the real clock, a sleeper that can be
/// woken, and real signals. `Serve` is the same loop with those swapped out.
pub fn run(
    mut config: Config,
    work: &dyn Work,
    beside: Option<&Worker<'_>>,
) -> Result<Outcome, ServeError> {
    // The path it was launched as: after a rename-over it names the new file.
    config.executable = config.executable.or_else(|| std::env::current_exe().ok());
    let doorbell = Doorbell::install(&config.doorbell, config.interval);
    let outcome = Serve {
        config: config.clone(),
        work,
        clock: &SystemClock,
        sleeper: &doorbell,
        shutdown: &Signals::install(),
        beside,
    }
    .run()?;

    // After `run`, which has let go of the lock the successor needs.
    if let Outcome::Served(report) = &outcome
        && report.hands_off()
        && let Some(exe) = &config.executable
    {
        // Its own arguments, so a scratch store survives the handoff.
        let args: Vec<OsString> = std::env::args_os().skip(1).collect();
        if let Err(error) = hand_off(&config, exe, &args, report.exit) {
            tracing::warn!(%error, executable = %exe.display(), "could not hand off; the next hook will start a server");
        }
    }
    Ok(outcome)
}

/// Start the build now at `exe` as this server's successor.
///
/// Spawn-then-exit rather than `exec`. The lock goes either way — its
/// descriptor is close-on-exec — so exec buys no gap-free handover, and it
/// would discard the log writer's unflushed tail: the lines saying why. A hook
/// that spawns a server in the gap just races the successor for the lock,
/// which `Serve::run` already settles.
///
/// Tried once, and the caller exits whatever happens: a build that cannot
/// start costs the old silence, never a loop.
fn hand_off(
    config: &Config,
    exe: &Path,
    args: &[impl AsRef<OsStr>],
    reason: Exit,
) -> Result<u32, ServeError> {
    let from = std::process::id();
    let pid = spawn_detached(exe, args)?;
    match claimed(&config.status, pid, HANDOFF_LOOKS, &RealSleeper) {
        true => {
            tracing::info!(from, to = pid, reason = reason.as_str(), executable = %exe.display(), "handed off to a new server")
        }
        // Slow, beaten to the lock by a hook's server, or failed to start.
        false => {
            tracing::warn!(from, to = pid, reason = reason.as_str(), executable = %exe.display(), "handed off, but the new server has not claimed the lock")
        }
    }
    Ok(pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FixedClock;
    use std::cell::Cell;
    use std::sync::atomic::AtomicU32;

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

    /// Moves the clock, and the wall a little, so a worker thread gets a look in.
    struct Walled<'a>(&'a FixedClock);

    impl Sleeper for Walled<'_> {
        fn sleep(&self, duration: Duration) {
            std::thread::sleep(Duration::from_millis(30));
            Fast(self.0).sleep(duration);
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
            beside: None,
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
        let report = serve(scratch.config(), &(|| Ok(Pulse::Idle.into())));

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
                    true => Ok(Pulse::Busy.into()),
                    false => Ok(Pulse::Idle.into()),
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
        let report = serve(config, &(|| Ok(Pulse::Busy.into())));

        assert_eq!(report.exit, Exit::MaxLifetime);
        assert_eq!(report.uptime, Duration::from_secs(60));
    }

    #[test]
    fn a_server_stands_down_when_its_binary_is_replaced() {
        // Found live: `make install` renames a new build over the old one,
        // and the running server kept pushing with the old code until its
        // 24h lifetime ran out.
        let scratch = Scratch::new("replaced");
        let exe = scratch.0.join("inkling");
        std::fs::write(&exe, "old build").unwrap();
        let config = Config {
            executable: Some(exe.clone()),
            ..scratch.config()
        };
        let ticks = Cell::new(0);
        let report = serve(
            config,
            &(|| {
                ticks.set(ticks.get() + 1);
                if ticks.get() == 3 {
                    let new = scratch.0.join("inkling.new");
                    std::fs::write(&new, "new build").unwrap();
                    std::fs::rename(&new, &exe).unwrap();
                }
                Ok(Pulse::Busy.into())
            }),
        );

        assert_eq!(report.exit, Exit::Replaced);
        assert_eq!(report.ticks, 3, "it noticed on the tick it happened");
    }

    #[test]
    fn a_replaced_server_with_work_hands_off() {
        // Found live: after `make install` the server stood down and nothing
        // polled until a hook happened to fire — never, for an idle session
        // waiting on CI.
        let scratch = Scratch::new("handoff");
        let exe = scratch.0.join("inkling");
        std::fs::write(&exe, "old build").unwrap();
        let config = Config {
            executable: Some(exe.clone()),
            ..scratch.config()
        };
        let report = serve(
            config,
            &(|| {
                std::fs::write(scratch.0.join("inkling.new"), "new build").unwrap();
                std::fs::rename(scratch.0.join("inkling.new"), &exe).unwrap();
                Ok(Pulse::Busy.into())
            }),
        );

        assert_eq!(report.exit, Exit::Replaced);
        assert!(report.hands_off());
    }

    #[test]
    fn a_replaced_server_with_nothing_to_do_just_stands_down() {
        let scratch = Scratch::new("handoff-idle");
        let exe = scratch.0.join("inkling");
        std::fs::write(&exe, "old build").unwrap();
        let config = Config {
            executable: Some(exe.clone()),
            ..scratch.config()
        };
        let report = serve(
            config,
            &(|| {
                std::fs::write(scratch.0.join("inkling.new"), "new build").unwrap();
                std::fs::rename(scratch.0.join("inkling.new"), &exe).unwrap();
                Ok(Pulse::Idle.into())
            }),
        );

        assert_eq!(report.exit, Exit::Replaced);
        assert!(
            !report.hands_off(),
            "the next hook with a reason starts one"
        );
    }

    /// Renames a new build over `exe` the first time it is asked to nap.
    struct InstallsDuringNap<'a> {
        clock: &'a FixedClock,
        exe: PathBuf,
    }

    impl Sleeper for InstallsDuringNap<'_> {
        fn sleep(&self, duration: Duration) {
            let new = self.exe.with_extension("new");
            std::fs::write(&new, "new build").unwrap();
            std::fs::rename(&new, &self.exe).unwrap();
            self.clock
                .advance(chrono::TimeDelta::from_std(duration).unwrap());
        }
    }

    #[test]
    fn a_build_installed_during_a_nap_takes_over_before_another_tick() {
        // Regression: the old server woke, ticked with the old code
        // against a store the new build had already written, and only then
        // looked at its executable.
        let scratch = Scratch::new("installed-napping");
        let exe = scratch.0.join("inkling");
        std::fs::write(&exe, "old build").unwrap();
        let clock = FixedClock::at("2026-09-20T12:00:00Z");
        let sleeper = InstallsDuringNap {
            clock: &clock,
            exe: exe.clone(),
        };
        let ticks = Cell::new(0);
        let work = || {
            ticks.set(ticks.get() + 1);
            Ok(Pulse::Busy.into())
        };
        let outcome = Serve {
            config: Config {
                executable: Some(exe),
                ..scratch.config()
            },
            work: &work,
            clock: &clock,
            sleeper: &sleeper,
            shutdown: &Never,
            beside: None,
        }
        .run()
        .unwrap();

        let Outcome::Served(report) = outcome else {
            panic!("nobody else held the lock")
        };
        assert_eq!(report.exit, Exit::Replaced);
        assert_eq!(
            ticks.get(),
            1,
            "no tick ran on the old code after the install"
        );
        assert!(report.hands_off());
    }

    #[test]
    fn a_server_older_than_the_store_stands_down_and_does_not_hand_off() {
        // Its own executable is the old build, so a successor would be
        // itself. The next hook from the newer build starts the right one.
        let scratch = Scratch::new("superseded");
        let report = serve(scratch.config(), &(|| Ok(Pulse::Superseded.into())));

        assert_eq!(report.exit, Exit::Superseded);
        assert_eq!(report.ticks, 1);
        assert!(!report.hands_off());
    }

    #[test]
    fn a_busy_server_at_its_lifetime_cap_hands_off() {
        let scratch = Scratch::new("handoff-lifetime");
        let config = Config {
            max_lifetime: Duration::from_secs(60),
            ..scratch.config()
        };
        let report = serve(config, &(|| Ok(Pulse::Busy.into())));

        assert_eq!(report.exit, Exit::MaxLifetime);
        assert!(report.hands_off());
    }

    #[test]
    fn an_idle_or_signalled_server_does_not_hand_off() {
        let scratch = Scratch::new("no-handoff");
        let report = serve(scratch.config(), &(|| Ok(Pulse::Idle.into())));
        assert_eq!(report.exit, Exit::Idle);
        assert!(!report.hands_off());

        let signalled = Report {
            exit: Exit::Signalled,
            busy: true,
            ..report
        };
        assert!(!signalled.hands_off(), "somebody asked it to stop");
    }

    #[test]
    fn a_handoff_to_a_build_that_cannot_run_fails_once() {
        // The fallback is today's stand-down: an error to log, not a retry.
        let scratch = Scratch::new("handoff-broken");
        let exe = scratch.0.join("inkling");
        std::fs::write(&exe, "not a program").unwrap();

        let error =
            hand_off(&scratch.config(), &exe, &["serve"], Exit::Replaced).expect_err("cannot run");
        assert!(matches!(error, ServeError::Spawn(_)), "{error}");
    }

    #[test]
    fn an_untouched_binary_changes_nothing() {
        let scratch = Scratch::new("untouched");
        let exe = scratch.0.join("inkling");
        std::fs::write(&exe, "build").unwrap();
        let config = Config {
            executable: Some(exe),
            ..scratch.config()
        };
        let report = serve(config, &(|| Ok(Pulse::Idle.into())));
        assert_eq!(report.exit, Exit::Idle);
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
                    _ => Ok(Pulse::Idle.into()),
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
                    _ => Ok(Pulse::Idle.into()),
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
            Ok(Pulse::Busy.into())
        };
        let outcome = Serve {
            config: scratch.config(),
            work: &work,
            clock: &clock,
            sleeper: &sleeper,
            shutdown: &signals,
            beside: None,
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
        let work = || Ok(Pulse::Idle.into());
        let outcome = Serve {
            config,
            work: &work,
            clock: &clock,
            sleeper: &sleeper,
            shutdown: &Never,
            beside: None,
        }
        .run()
        .expect("standing down is not an error");

        assert!(matches!(outcome, Outcome::AlreadyRunning));
    }

    #[test]
    fn a_worker_beside_the_loop_runs_while_it_serves_and_stops_with_it() {
        let scratch = Scratch::new("beside");
        let clock = FixedClock::at("2026-09-20T12:00:00Z");
        let sleeper = Fast(&clock);
        let rounds = AtomicBool::new(false);
        let worker = |beside: &Beside| {
            while !beside.stopping() {
                rounds.store(true, Ordering::Relaxed);
                beside.nap(Duration::from_secs(60));
            }
        };
        // Busy until the worker has been round once, so the loop can't end
        // before the thread has started.
        let work = || match rounds.load(Ordering::Relaxed) {
            true => Ok(Pulse::Idle.into()),
            false => Ok(Pulse::Busy.into()),
        };
        let outcome = Serve {
            config: scratch.config(),
            work: &work,
            clock: &clock,
            sleeper: &sleeper,
            shutdown: &Never,
            beside: Some(&worker),
        }
        .run()
        .expect("served");

        assert!(matches!(outcome, Outcome::Served(_)), "and did not hang");
    }

    #[test]
    fn waking_the_loop_wakes_the_worker_too() {
        // A watch registered while the worker naps must be read before a
        // reply to it can land, or the reply becomes the silent baseline.
        let scratch = Scratch::new("beside-woken");
        let clock = FixedClock::at("2026-09-20T12:00:00Z");
        let rounds = AtomicU32::new(0);
        let worker = |beside: &Beside| {
            while !beside.stopping() {
                rounds.fetch_add(1, Ordering::Relaxed);
                beside.nap(Duration::from_secs(60));
            }
        };
        let work = || match rounds.load(Ordering::Relaxed) {
            0 | 1 => Ok(Pulse::Busy.into()),
            _ => Ok(Pulse::Idle.into()),
        };
        let outcome = Serve {
            config: Config {
                max_lifetime: Duration::from_secs(30),
                ..scratch.config()
            },
            work: &work,
            clock: &clock,
            sleeper: &Walled(&clock),
            shutdown: &Never,
            beside: Some(&worker),
        }
        .run()
        .expect("served");

        let Outcome::Served(report) = outcome else {
            panic!("stood down")
        };
        assert_eq!(report.exit, Exit::Idle, "the worker slept through the loop");
    }

    #[test]
    fn a_server_that_stands_down_starts_no_worker() {
        // The worker reads Slack; two of them would both pay for a reading.
        let scratch = Scratch::new("beside-contended");
        let config = scratch.config();
        let _held = FileLock::try_acquire(&config.lock).unwrap();
        let clock = FixedClock::at("2026-09-20T12:00:00Z");
        let started = AtomicBool::new(false);
        let worker = |_: &Beside| started.store(true, Ordering::Relaxed);
        Serve {
            config,
            work: &|| Ok(Pulse::Idle.into()),
            clock: &clock,
            sleeper: &Fast(&clock),
            shutdown: &Never,
            beside: Some(&worker),
        }
        .run()
        .unwrap();
        assert!(!started.load(Ordering::Relaxed));
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

    /// Naps that cost nothing, for the waiting a spawn does.
    struct Unslept;

    impl Sleeper for Unslept {
        fn sleep(&self, _: Duration) {}
    }

    #[test]
    fn a_spawn_waits_for_the_child_to_claim_the_lock() {
        let scratch = Scratch::new("takeoff");
        let config = scratch.config();
        assert!(
            !claimed(&config.status, 4242, TAKEOFF_LOOKS, &Unslept),
            "nothing has claimed anything"
        );

        atomically(
            &config.status,
            &Record {
                pid: 4242,
                started_at: "2026-09-20T12:00:00Z".parse().unwrap(),
                last_tick_at: None,
                ticks: 0,
                failures: 0,
                last_error: None,
            },
        )
        .unwrap();

        assert!(claimed(&config.status, 4242, TAKEOFF_LOOKS, &Unslept));
        assert!(
            !claimed(&config.status, 9999, TAKEOFF_LOOKS, &Unslept),
            "and a record left by an earlier server is not this one arriving"
        );
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
    fn a_server_records_itself_before_its_first_tick() {
        // Regression: `inkling status` straight after the `watch` that
        // spawned a server said `serving no`. A tick can take a second, and the
        // claim on the lock is news before then.
        let scratch = Scratch::new("claims");
        let config = scratch.config();
        let status = config.status.clone();
        let seen = std::cell::RefCell::new(Vec::new());
        serve(
            config,
            &(|| {
                seen.borrow_mut()
                    .push(read_record(&status).map(|record| (record.pid, record.ticks)));
                Ok(Pulse::Idle.into())
            }),
        );

        assert_eq!(
            seen.borrow().first().copied().flatten(),
            Some((std::process::id(), 0)),
            "the first tick should already find the claim on file"
        );
    }

    /// Every tick's time, for asserting how they are spaced.
    fn ticks_at(config: Config, next: impl Fn(DateTime<Utc>) -> Beat) -> Vec<DateTime<Utc>> {
        let clock = FixedClock::at("2026-09-20T12:00:00Z");
        let sleeper = Fast(&clock);
        let times = std::cell::RefCell::new(Vec::new());
        let work = || {
            let now = clock.now();
            times.borrow_mut().push(now);
            Ok(next(now))
        };
        Serve {
            config,
            work: &work,
            clock: &clock,
            sleeper: &sleeper,
            shutdown: &Never,
            beside: None,
        }
        .run()
        .expect("served");
        times.into_inner()
    }

    fn gaps(times: &[DateTime<Utc>]) -> Vec<i64> {
        times
            .windows(2)
            .map(|w| (w[1] - w[0]).num_seconds())
            .collect()
    }

    #[test]
    fn a_server_sleeps_until_its_work_is_next_due() {
        // The saving: a server whose watches are quiet wakes when one of them
        // is due, not every interval.
        let scratch = Scratch::new("sleeps");
        let config = Config {
            max_lifetime: Duration::from_secs(3600),
            ..scratch.config()
        };
        let times = ticks_at(config, |now| Beat {
            pulse: Pulse::Busy,
            next: Some(now + chrono::TimeDelta::minutes(20)),
        });
        assert_eq!(gaps(&times), [1200, 1200, 1200]);
    }

    #[test]
    fn a_server_never_ticks_faster_than_its_interval() {
        // A watch that can't be read stays due; without a floor that is a spin.
        let scratch = Scratch::new("floor");
        let config = Config {
            max_lifetime: Duration::from_secs(5),
            ..scratch.config()
        };
        let times = ticks_at(config, |now| Beat {
            pulse: Pulse::Busy,
            next: Some(now - chrono::TimeDelta::minutes(5)),
        });
        assert_eq!(gaps(&times), [1, 1, 1, 1, 1]);
    }

    #[test]
    fn a_long_sleep_still_ends_at_the_idle_deadline() {
        let scratch = Scratch::new("idle-deadline");
        let times = ticks_at(scratch.config(), |now| Beat {
            pulse: Pulse::Idle,
            next: Some(now + chrono::TimeDelta::hours(1)),
        });
        assert_eq!(gaps(&times), [10], "idle for 10s, then gone");
    }

    #[test]
    fn a_long_sleep_still_ends_at_the_lifetime_cap() {
        let scratch = Scratch::new("lifetime-deadline");
        let config = Config {
            max_lifetime: Duration::from_secs(90),
            ..scratch.config()
        };
        let times = ticks_at(config, |now| Beat {
            pulse: Pulse::Busy,
            next: Some(now + chrono::TimeDelta::hours(1)),
        });
        assert_eq!(gaps(&times), [90]);
    }

    #[test]
    fn a_rung_doorbell_cuts_a_nap_short() {
        // How a new watch is read promptly by a server sleeping for an hour.
        let scratch = Scratch::new("doorbell");
        let config = scratch.config();
        let bell = Doorbell::install(&config.doorbell, Duration::from_secs(60));

        let started = std::time::Instant::now();
        bell.sleep(Duration::from_millis(50));
        assert!(
            started.elapsed() >= Duration::from_millis(50),
            "unrung, it sleeps"
        );

        ring(&config.doorbell);
        let started = std::time::Instant::now();
        bell.sleep(Duration::from_secs(30));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );

        let started = std::time::Instant::now();
        bell.sleep(Duration::from_millis(50));
        assert!(
            started.elapsed() >= Duration::from_millis(50),
            "one ring wakes it once"
        );
    }

    #[test]
    fn a_nap_is_never_rounded_short() {
        // Found live: waking a fraction of a millisecond before a watch was
        // due found nothing due, and cost a whole interval.
        let scratch = Scratch::new("rounding");
        let bell = Doorbell::install(&scratch.config().doorbell, Duration::from_secs(60));
        let nap = Duration::from_micros(1900);
        let started = std::time::Instant::now();
        bell.sleep(nap);
        assert!(started.elapsed() >= nap, "{:?}", started.elapsed());
    }

    #[test]
    fn ringing_with_nobody_home_is_harmless() {
        let scratch = Scratch::new("nobody");
        let config = scratch.config();
        ring(&config.doorbell); // no such file
        drop(Doorbell::install(&config.doorbell, Duration::from_secs(60)));
        let started = std::time::Instant::now();
        ring(&config.doorbell); // a FIFO with no reader
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn without_a_doorbell_a_nap_is_cut_to_the_fallback() {
        // Somewhere a FIFO can't be made, nothing can wake a server, so it
        // keeps its old cadence rather than sleeping through a new watch.
        let bell = Doorbell::install(
            Path::new("/nonexistent/dir/serve.wake"),
            Duration::from_millis(20),
        );
        let started = std::time::Instant::now();
        bell.sleep(Duration::from_secs(30));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn the_running_server_records_itself() {
        let scratch = Scratch::new("record");
        let config = scratch.config();
        let status = config.status.clone();
        serve(config, &(|| Ok(Pulse::Idle.into())));

        let record = read_record(&status).expect("a record was written");
        assert_eq!(record.pid, std::process::id());
        assert_eq!(record.ticks, 11);
        assert!(record.last_tick_at.is_some());
    }
}
