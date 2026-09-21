//! Running a command with a deadline.
//!
//! `Command::output` waits as long as the child takes, and both callers here
//! sit on a path something else is waiting for: the resident server's tick,
//! and the refresh a prompt hook pays for. A `gh auth token` blocked on a
//! keychain, or an `acli` that never answers, would otherwise stop the tick
//! for good — which is the shape of failure a notification tool is least
//! allowed to have.

use std::io;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// How often to look in on the child. Short enough to add nothing measurable
/// to a fast command, long enough not to spin.
const LOOK: Duration = Duration::from_millis(10);

/// Run to completion, or kill the child once `budget` has passed.
///
/// Output is collected at the end rather than as it arrives, so a child that
/// writes more than a pipe buffer stalls until the deadline kills it. Both
/// callers read a token or a small JSON object, and the deadline is the
/// backstop either way.
pub fn output_within(command: &mut Command, budget: Duration) -> io::Result<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let deadline = Instant::now() + budget;
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            return Err(gave_up(&mut child, budget));
        }
        std::thread::sleep(LOOK);
    }
    child.wait_with_output()
}

/// Kill it and reap it, so the caller's process doesn't collect zombies.
fn gave_up(child: &mut Child, budget: Duration) -> io::Error {
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
    fn a_missing_binary_fails_before_any_waiting() {
        let error = output_within(
            &mut Command::new("no-such-binary-here"),
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
