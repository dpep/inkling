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

/// Run to completion, or kill the child and everything it started once
/// `budget` has passed.
///
/// Done means exited *and* its output closed: a background process that
/// inherited stdout holds the pipe open after the child is gone, and the
/// deadline covers that wait too.
pub fn output_within(command: &mut Command, budget: Duration) -> io::Result<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own group, so giving up reaches what it started. The price: a
        // Ctrl-C at the terminal no longer reaches it.
        .process_group(0)
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
            return Err(gave_up(&mut child, budget));
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

/// Kill its whole process group and reap it, so neither its descendants nor
/// zombies outlive the call.
fn gave_up(child: &mut Child, budget: Duration) -> io::Error {
    // The group is the child's pid: `process_group(0)` made it the leader.
    unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
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
        output_within(&mut sh(&script), Duration::from_millis(300)).unwrap_err();

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
