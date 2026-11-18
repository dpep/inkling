//! Running a command with a deadline.
//!
//! `Command::output` waits as long as the child takes, and both callers here
//! sit on a path something else is waiting for: the resident server's tick,
//! and the refresh a prompt hook pays for. A `gh auth token` blocked on a
//! keychain, or an `acli` that never answers, would otherwise stop the tick
//! for good — which is the shape of failure a notification tool is least
//! allowed to have.

use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Output, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often to look in on the child. Short enough to add nothing measurable
/// to a fast command, long enough not to spin.
const LOOK: Duration = Duration::from_millis(10);

/// Run to completion, or kill the child once `budget` has passed — and
/// everything it started, unless run from a terminal's foreground.
///
/// Done means exited *and* its output closed: a background process that
/// inherited stdout holds the pipe open after the child is gone, and the
/// deadline covers that wait too.
pub fn output_within(command: &mut Command, budget: Duration) -> io::Result<Output> {
    // Its own group, so giving up reaches what it started. Not in the
    // terminal's foreground, though: a background group that reads the tty is
    // stopped, and a token command may prompt there. There, only the child
    // itself is killed, and a Ctrl-C reaches it.
    run(command, budget, !in_foreground())
}

fn run(command: &mut Command, budget: Duration, grouped: bool) -> io::Result<Output> {
    if grouped {
        command.process_group(0);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    let deadline = Instant::now() + budget;
    loop {
        if let Some(status) = child.try_wait()?
            && stdout.is_finished()
            && stderr.is_finished()
        {
            return Ok(Output {
                status,
                stdout: stdout.join().unwrap_or_default(),
                stderr: stderr.join().unwrap_or_default(),
            });
        }
        if Instant::now() >= deadline {
            // The drains are left to end when the pipe closes: something that
            // left the group can hold it for as long as it likes.
            return Err(gave_up(&mut child, grouped, budget));
        }
        std::thread::sleep(LOOK);
    }
}

/// Read a pipe to its end on a thread of its own, so a child that writes more
/// than a pipe buffer never stalls waiting for us.
fn drain(pipe: Option<impl Read + Send + 'static>) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    })
}

/// Whether this process runs in the foreground of a controlling terminal.
fn in_foreground() -> bool {
    use std::os::fd::AsRawFd;
    match std::fs::File::open("/dev/tty") {
        Ok(tty) => unsafe { libc::tcgetpgrp(tty.as_raw_fd()) == libc::getpgrp() },
        Err(_) => false,
    }
}

/// Kill it, and its whole process group when it has one, and reap it, so
/// neither its descendants nor zombies outlive the call.
fn gave_up(child: &mut Child, grouped: bool, budget: Duration) -> io::Error {
    if grouped {
        // The group is the child's pid: `process_group(0)` made it the leader.
        unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
    }
    let _ = child.kill();
    let _ = child.wait();
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!("no answer within {}", humantime::format_duration(budget)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command
    }

    #[test]
    fn a_command_that_answers_is_read_normally() {
        let output = output_within(&mut sh("echo hello"), Duration::from_secs(5)).unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
    }

    #[test]
    fn a_command_that_never_answers_is_killed_rather_than_waited_on() {
        let started = Instant::now();
        let error = output_within(&mut sh("sleep 30"), Duration::from_millis(80)).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("within 80ms"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_failing_command_is_still_an_answer() {
        // Distinct from a hang: the caller decides what a non-zero exit means.
        let output = output_within(&mut sh("exit 7"), Duration::from_secs(5)).unwrap();
        assert_eq!(output.status.code(), Some(7));
    }

    #[test]
    fn more_output_than_a_pipe_holds_is_read_in_full() {
        let output =
            output_within(&mut sh("head -c 200000 /dev/zero"), Duration::from_secs(5)).unwrap();
        assert_eq!(output.stdout.len(), 200000);
    }

    #[test]
    fn a_background_process_holding_the_output_does_not_outlast_the_deadline() {
        // Found by the hunt: a token command that forked something which
        // inherited stdout held a 2s call for as long as that ran.
        let started = Instant::now();
        let error =
            output_within(&mut sh("echo tok; sleep 5 &"), Duration::from_millis(200)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn giving_up_kills_what_the_command_started_too() {
        let pidfile = std::env::temp_dir().join(format!("inkling-group-{}", std::process::id()));
        let script = format!("sleep 30 & echo $! > {}; wait", pidfile.display());
        // Grouped whether or not the tests run from a terminal.
        run(&mut sh(&script), Duration::from_millis(300), true).unwrap_err();

        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        std::fs::remove_file(&pidfile).unwrap();
        // Reaped by init once killed; a moment to get there.
        std::thread::sleep(Duration::from_millis(100));
        assert_ne!(
            unsafe { libc::kill(pid, 0) },
            0,
            "sleep {pid} outlived the call"
        );
    }

    /// Run from a terminal, a command may prompt on it: a token command
    /// asking for a passphrase. Reruns this test under a pseudo-terminal
    /// that `script` gives the foreground, and types the answer.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_command_can_prompt_on_the_terminal_it_was_run_from() {
        use std::io::Write;
        const NAME: &str =
            "subprocess::tests::a_command_can_prompt_on_the_terminal_it_was_run_from";
        if std::env::var_os("INKLING_TEST_IN_PTY").is_some() {
            let script = "printf 'passphrase: ' > /dev/tty; read x < /dev/tty; echo tok$x";
            let output = output_within(&mut sh(script), Duration::from_secs(3)).unwrap();
            println!(
                "answered {}",
                String::from_utf8_lossy(&output.stdout).trim()
            );
            return;
        }
        let mut pty = Command::new("script")
            .args(["-q", "/dev/null"])
            .arg(std::env::current_exe().unwrap())
            .args([NAME, "--exact", "--nocapture", "--test-threads=1"])
            .env("INKLING_TEST_IN_PTY", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        // Held open until the end: `script` may stop at end of input.
        let mut typed = pty.stdin.take().unwrap();
        typed.write_all(b"x\n").unwrap();
        let mut seen = String::new();
        pty.stdout
            .take()
            .unwrap()
            .read_to_string(&mut seen)
            .unwrap();
        drop(typed);
        pty.wait().unwrap();
        assert!(seen.contains("answered tokx"), "{seen}");
    }

    #[test]
    fn a_missing_binary_fails_before_any_waiting() {
        let error = output_within(
            &mut Command::new("no-such-binary-here"),
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
