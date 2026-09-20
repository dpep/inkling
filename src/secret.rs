//! A value that must never end up in a log line.
//!
//! Tokens get carried through structs that derive `Debug`, and one stray
//! `{:?}` is all it takes. Wrapping them makes the leak impossible rather
//! than merely discouraged: reading the value takes an explicit `expose()`.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Serializable on purpose, and only on purpose: an attached session's token
/// has to live in `watches.json` beside the address it belongs to, and that
/// file is written 0600. Serialization was never where tokens leaked —
/// `Debug` was, and it stays redacted.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Secret(String);

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("environment variable {0} is not set")]
    MissingEnv(String),
    #[error("running `{command}`: {source}")]
    Command {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("`{command}` exited {code} without producing a token")]
    CommandFailed { command: String, code: String },
    #[error("no credential configured: set token_env or token_command")]
    NotConfigured,
}

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The only way to read it. Grep-able, so a leak is easy to audit for.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn from_env(name: &str) -> Result<Self, SecretError> {
        std::env::var(name)
            .map(Self::new)
            .map_err(|_| SecretError::MissingEnv(name.to_string()))
    }

    /// Run a command and take its stdout, for `gh auth token` and friends.
    pub fn from_command(command: &str) -> Result<Self, SecretError> {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .output()
            .map_err(|source| SecretError::Command {
                command: command.to_string(),
                source,
            })?;

        if !output.status.success() {
            return Err(SecretError::CommandFailed {
                command: command.to_string(),
                code: output
                    .status
                    .code()
                    .map_or_else(|| "on a signal".into(), |c| c.to_string()),
            });
        }
        Ok(Self::new(String::from_utf8_lossy(&output.stdout).trim()))
    }

    /// Resolve from config: an environment variable, or a command, in that
    /// order. The token itself is never a config value.
    pub fn resolve(
        token_env: Option<&str>,
        token_command: Option<&str>,
    ) -> Result<Self, SecretError> {
        match (token_env, token_command) {
            (Some(name), _) => Self::from_env(name),
            (None, Some(command)) => Self::from_command(command),
            (None, None) => Err(SecretError::NotConfigured),
        }
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(redacted)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("redacted")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_display_never_reveal_the_value() {
        let secret = Secret::new("ghp_supersecrettoken");
        assert_eq!(format!("{secret:?}"), "Secret(redacted)");
        assert_eq!(format!("{secret}"), "redacted");
        assert!(!format!("{secret:?} {secret}").contains("ghp_"));
    }

    #[test]
    fn a_secret_nested_in_a_derived_debug_stays_redacted() {
        // The realistic leak: a struct that derives Debug and happens to
        // carry a token.
        #[derive(Debug)]
        #[allow(dead_code)]
        struct Config {
            url: String,
            token: Secret,
        }
        let config = Config {
            url: "https://api.github.com".into(),
            token: Secret::new("ghp_supersecrettoken"),
        };
        assert!(!format!("{config:?}").contains("ghp_"));
    }

    #[test]
    fn exposing_returns_the_real_value() {
        assert_eq!(Secret::new("abc").expose(), "abc");
    }

    #[test]
    fn a_secret_round_trips_as_a_plain_string() {
        // What the state file holds. Redaction is Debug's job; a stored token
        // that came back as "redacted" would be a token that never worked.
        let json = serde_json::to_string(&Secret::new("tok_abc123")).unwrap();
        assert_eq!(json, r#""tok_abc123""#);
        assert_eq!(
            serde_json::from_str::<Secret>(&json).unwrap().expose(),
            "tok_abc123"
        );
    }

    #[test]
    fn a_command_supplies_its_trimmed_output() {
        let secret = Secret::from_command("echo ghp_from_command").unwrap();
        assert_eq!(secret.expose(), "ghp_from_command");
    }

    #[test]
    fn a_failing_command_is_an_error_not_an_empty_token() {
        let error = Secret::from_command("exit 3").unwrap_err();
        assert!(matches!(error, SecretError::CommandFailed { .. }));
    }

    #[test]
    fn resolving_without_a_source_is_an_error() {
        assert!(matches!(
            Secret::resolve(None, None),
            Err(SecretError::NotConfigured)
        ));
    }

    #[test]
    fn resolving_prefers_the_environment_variable() {
        assert!(matches!(
            Secret::resolve(Some("INKLING_DEFINITELY_UNSET_VAR"), Some("echo fallback")),
            Err(SecretError::MissingEnv(_))
        ));
    }
}
