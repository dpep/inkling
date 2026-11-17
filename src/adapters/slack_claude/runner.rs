//! Running `claude` as a subprocess, with only the tools a call needs.
//!
//! The transport is a language model with untrusted Slack text flowing
//! through it, so "Inkling never posts" cannot rest on prompt wording — a
//! crafted message could try to talk the bridge into sending one. Every
//! invocation allowlists exactly the tools that call requires, which makes
//! the promise a capability rather than an instruction.

use std::io;
use std::process::Command;
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
    /// `attempt` is 0 for the first try. Later attempts vary the cached
    /// prefix so a retry can't be served the same poisoned tool list.
    fn run(
        &self,
        prompt: &str,
        allowed_tools: &[&str],
        attempt: usize,
    ) -> Result<String, RunnerError>;
}

/// Replaces Claude Code's default system prompt, which is written for
/// interactive coding and is pure overhead here.
///
/// Must stay constant across calls. The prompt cache is keyed on this prefix,
/// and a warm cache is the difference between $0.018 and $0.059 per call —
/// interpolating anything per-call into it (a channel, a timestamp) would
/// break every cache hit. Per-call variation goes in the user message.
const SYSTEM_PROMPT: &str = "You execute exactly one MCP tool call and return \
     its result in the requested format. No commentary, no follow-up actions.";

pub struct ClaudeRunner {
    pub command: String,
    pub model: Option<String>,
    pub timeout: Duration,
    /// An MCP config naming exactly the servers this bridge needs. When set,
    /// the subprocess is scoped strictly to them.
    ///
    /// `--allowedTools` gates what may be *called*, not what is *loaded*, so
    /// a heavy ambient configuration still pays for every server's schemas —
    /// enough, in some environments, to blow the context limit before the
    /// call runs. This is the only way to stop paying for servers we don't
    /// use.
    pub mcp_config: Option<String>,
}

impl Default for ClaudeRunner {
    fn default() -> Self {
        Self {
            command: "claude".into(),
            // The bridge executes tools; it decides nothing. A small model is
            // both correct and several times cheaper.
            model: Some("haiku".into()),
            timeout: Duration::from_secs(180),
            mcp_config: None,
        }
    }
}

impl ClaudeRunner {
    /// Flags that keep the boot cost down. Measured against the default
    /// invocation: 6x cheaper and 2x faster (see docs/slack-mcp-spike.md).
    ///
    /// `--tools ""` drops every built-in tool definition — the bridge needs
    /// none of them, and their descriptions are large. Not `--bare`, which
    /// looks apt but breaks Anthropic auth.
    fn args(&self, prompt: &str, allowed_tools: &[&str], attempt: usize) -> Vec<String> {
        // The marker goes in the *system* prompt, not the user prompt.
        // Caching is prefix-based and tool definitions sit after the system
        // prompt but before the user message — appending to the latter would
        // leave the cached tool payload untouched, which is the one thing a
        // retry needs to change. Only retries pay for the miss.
        let system = match attempt {
            0 => SYSTEM_PROMPT.to_string(),
            n => format!("{SYSTEM_PROMPT} (attempt {})", n + 1),
        };
        let mut args = vec![
            "-p".into(),
            prompt.into(),
            "--output-format".into(),
            "json".into(),
            "--tools".into(),
            String::new(),
            "--system-prompt".into(),
            system,
            "--allowedTools".into(),
        ];
        args.extend(allowed_tools.iter().map(|t| (*t).to_string()));
        if let Some(model) = &self.model {
            args.push("--model".into());
            args.push(model.clone());
        }
        if let Some(config) = &self.mcp_config {
            // Both flags or neither: --mcp-config alone would *add* to the
            // ambient configuration rather than replace it, which is the
            // opposite of the point.
            args.push("--mcp-config".into());
            args.push(config.clone());
            args.push("--strict-mcp-config".into());
        }
        args
    }
}

/// Set on every `claude` this spawns. The child is a Claude Code session and
/// runs the user's hooks, inkling's among them; this is how they know to do
/// nothing, rather than subscribing the bridge and watching what it read.
pub const BRIDGE_ENV: &str = "INKLING_BRIDGE";

impl ClaudeRunner {
    fn command(&self, prompt: &str, allowed_tools: &[&str], attempt: usize) -> Command {
        let mut command = Command::new(&self.command);
        command
            .args(self.args(prompt, allowed_tools, attempt))
            .env(BRIDGE_ENV, "1");
        command
    }
}

impl Runner for ClaudeRunner {
    fn run(
        &self,
        prompt: &str,
        allowed_tools: &[&str],
        attempt: usize,
    ) -> Result<String, RunnerError> {
        // Bounded: a watch read happens on the resident server's tick, which
        // a hung `claude` would otherwise stop for good.
        let output = crate::subprocess::output_within(
            &mut self.command(prompt, allowed_tools, attempt),
            self.timeout,
        )
        .map_err(|source| match source.kind() {
            io::ErrorKind::TimedOut => RunnerError::TimedOut {
                command: self.command.clone(),
                timeout: self.timeout,
            },
            _ => RunnerError::Spawn {
                command: self.command.clone(),
                source,
            },
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
    fn the_invocation_drops_the_default_prompt_and_tools() {
        // These three flags are the whole cost optimization; a regression here
        // is a 6x price increase that nothing else would catch.
        let args = ClaudeRunner::default().args("do the thing", &["some_tool"], 0);
        let pairs: Vec<_> = args.windows(2).map(|w| (&w[0], &w[1])).collect();

        assert!(pairs.contains(&(&"--tools".to_string(), &String::new())));
        assert!(pairs.contains(&(&"--model".to_string(), &"haiku".to_string())));
        assert!(args.iter().any(|a| a == SYSTEM_PROMPT));
        assert!(pairs.contains(&(&"--allowedTools".to_string(), &"some_tool".to_string())));
    }

    #[test]
    fn an_mcp_config_is_always_paired_with_strict() {
        // --mcp-config on its own adds servers instead of replacing them, so
        // the scoping would silently not happen.
        let runner = ClaudeRunner {
            mcp_config: Some(r#"{"mcpServers":{}}"#.into()),
            ..ClaudeRunner::default()
        };
        let args = runner.args("x", &["t"], 0);
        assert!(args.iter().any(|a| a == "--mcp-config"));
        assert!(args.iter().any(|a| a == "--strict-mcp-config"));
    }

    #[test]
    fn without_an_mcp_config_neither_flag_appears() {
        let args = ClaudeRunner::default().args("x", &["t"], 0);
        assert!(!args.iter().any(|a| a.starts_with("--mcp-config")));
        assert!(!args.iter().any(|a| a == "--strict-mcp-config"));
    }

    #[test]
    fn only_the_named_tools_are_allowed() {
        let args = ClaudeRunner::default().args("x", &["a", "b"], 0);
        let allowed = args.iter().position(|a| a == "--allowedTools").unwrap();
        assert_eq!(&args[allowed + 1..allowed + 3], ["a", "b"]);
    }

    #[test]
    fn a_retry_varies_the_cached_prefix() {
        // Identical retries get served the same cached tool definitions, so
        // a bad list poisons every attempt in a run. Varying the system
        // prompt — which precedes the tools — forces a fresh handshake.
        let runner = ClaudeRunner::default();
        let first = runner.args("x", &["t"], 0);
        let second = runner.args("x", &["t"], 1);

        assert!(first.iter().any(|a| a == SYSTEM_PROMPT));
        assert!(!second.iter().any(|a| a == SYSTEM_PROMPT));
        assert!(second.iter().any(|a| a.contains("(attempt 2)")));
    }

    #[test]
    fn the_child_is_told_it_is_the_bridge() {
        // Its hooks are the user's, inkling's included; without this every
        // bridge call would subscribe a throwaway session.
        let command = ClaudeRunner::default().command("x", &["t"], 0);
        let marked = command
            .get_envs()
            .any(|(key, value)| key == BRIDGE_ENV && value.is_some());
        assert!(marked);
    }

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
