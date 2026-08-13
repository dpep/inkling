//! Running `claude` as a subprocess, with only the tools a call needs.
//!
//! The transport is a language model with untrusted Slack text flowing
//! through it, so "Inkling never posts" cannot rest on prompt wording — a
//! crafted message could try to talk the bridge into sending one. Every
//! invocation allowlists exactly the tools that call requires, which makes
//! the promise a capability rather than an instruction.

use std::io;
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error("spawning `{command}`: {source}")]
    Spawn {
        command: String,
        #[source]
        source: io::Error,
    },
    #[error("`{command}` produced no usable output (exit {code})")]
    Failed { command: String, code: String },
    #[error("`{command}` timed out after {}s", timeout.as_secs())]
    TimedOut { command: String, timeout: Duration },
    #[error("could not read the claude envelope: {0}")]
    Envelope(String),
}

/// Runs a prompt and returns the model's reply. Behind a trait so tests
/// replay recorded transcripts instead of spending money and time.
pub trait Runner {
    fn run(&self, prompt: &str, allowed_tools: &[&str]) -> Result<String, RunnerError>;
}

pub struct ClaudeRunner {
    pub command: String,
    pub model: Option<String>,
    pub timeout: Duration,
}

impl Default for ClaudeRunner {
    fn default() -> Self {
        Self {
            command: "claude".into(),
            // The bridge executes tools; it does not decide anything. A small
            // model is both correct and several times cheaper.
            model: Some("haiku".into()),
            timeout: Duration::from_secs(180),
        }
    }
}

impl Runner for ClaudeRunner {
    fn run(&self, prompt: &str, allowed_tools: &[&str]) -> Result<String, RunnerError> {
        let mut command = Command::new(&self.command);
        command
            .arg("-p")
            .arg(prompt)
            .arg("--output-format")
            .arg("json")
            .arg("--allowedTools")
            .args(allowed_tools);
        if let Some(model) = &self.model {
            command.arg("--model").arg(model);
        }

        let output =
            command
                .stdin(Stdio::null())
                .output()
                .map_err(|source| RunnerError::Spawn {
                    command: self.command.clone(),
                    source,
                })?;

        if !output.status.success() {
            return Err(RunnerError::Failed {
                command: self.command.clone(),
                code: output
                    .status
                    .code()
                    .map_or_else(|| "signal".into(), |c| c.to_string()),
            });
        }
        extract_result(&String::from_utf8_lossy(&output.stdout))
    }
}

/// `--output-format json` wraps the reply in an envelope. Only `.result`
/// matters; the rest is cost and timing telemetry.
pub fn extract_result(stdout: &str) -> Result<String, RunnerError> {
    let envelope: serde_json::Value =
        serde_json::from_str(stdout).map_err(|e| RunnerError::Envelope(e.to_string()))?;

    if envelope.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
        return Err(RunnerError::Envelope(
            envelope
                .get("result")
                .and_then(|v| v.as_str())
                .unwrap_or("claude reported an error")
                .to_string(),
        ));
    }
    envelope
        .get("result")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or_else(|| RunnerError::Envelope("envelope had no result field".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_result_is_lifted_out_of_the_envelope() {
        let stdout =
            r#"{"type":"result","subtype":"success","total_cost_usd":0.08,"result":"hello"}"#;
        assert_eq!(extract_result(stdout).unwrap(), "hello");
    }

    #[test]
    fn an_error_envelope_becomes_an_error() {
        let stdout = r#"{"is_error":true,"result":"tool denied"}"#;
        assert!(extract_result(stdout).is_err());
    }

    #[test]
    fn non_json_output_is_an_error_rather_than_a_panic() {
        assert!(extract_result("claude: command not found").is_err());
        assert!(extract_result("").is_err());
    }
}
