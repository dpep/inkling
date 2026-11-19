//! Configuration: where credentials come from, how Slack is reached, and how
//! the resident server paces itself. Every field has a default.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub slack: Slack,
    #[serde(default)]
    pub github: GitHub,
    #[serde(default)]
    pub serve: Serve,
}

/// Keys only the removed Slack reactions read. Dropped before the strict
/// parse, so a config written for them still loads while any other unknown
/// key is still a typo worth refusing — and the refusal doesn't list them.
pub const RETIRED: &[&str] = &[
    "window",
    "tracking",
    "filters",
    "status",
    "run",
    "slack.personal",
    "slack.channels",
    "slack.mcp.tools.search",
];

/// Remove a dotted key from a parsed table, if it is there.
fn strip(table: &mut toml::de::DeTable<'_>, key: &str) {
    match key.split_once('.') {
        None => {
            table.remove(key);
        }
        Some((head, rest)) => {
            if let Some(toml::de::DeValue::Table(inner)) = table.get_mut(head).map(|v| v.get_mut())
            {
                strip(inner, rest);
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Slack {
    /// Slack user ID, e.g. `U012ABC345`: a channel message counts as a reply
    /// only when it mentions you. Resolved from the credentials and cached
    /// when omitted — nobody knows their own Slack ID off-hand.
    #[serde(default)]
    pub user_id: Option<String>,
    /// Which Slack MCP server to load, and what its tools are called.
    ///
    /// Omitted, the bridge inherits whatever MCP configuration the ambient
    /// environment has — which is what it has always done, and which makes
    /// reliability depend on things Inkling can't see.
    #[serde(default)]
    pub mcp: Option<SlackMcp>,
}

/// A declarable MCP server, named explicitly so the subprocess can be scoped
/// to it and nothing else.
///
/// `--allowedTools` restricts what the model may *call*; it does not restrict
/// what gets *loaded*. A heavy ambient configuration pulls every server's tool
/// schemas into the subprocess regardless, which can exceed the context limit
/// before the call runs. Naming one server here lets the bridge pass
/// `--strict-mcp-config` and collapse that payload.
///
/// Only works for a server expressible as a static block. An account-linked
/// OAuth connector isn't declarable, and `--strict-mcp-config` would drop it
/// along with everything else.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SlackMcp {
    /// Name the server is given in the generated config, and therefore the
    /// middle segment of every `mcp__<server>__<tool>` name.
    pub server: String,
    pub url: String,
    /// Values may reference the environment as `${VAR}`; a literal secret in
    /// a config file is exactly what this avoids.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub tools: SlackMcpTools,
}

/// Tool names by role. Defaults are the names the current server uses, so a
/// backend that matches needs none of this. `react` and `send_message` are
/// never called: they name the session's own calls, for the post-tool hook.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SlackMcpTools {
    #[serde(default = "d_read_channel")]
    pub read_channel: String,
    #[serde(default = "d_react")]
    pub react: String,
    #[serde(default = "d_search_channels")]
    pub search_channels: String,
    #[serde(default = "d_read_thread")]
    pub read_thread: String,
    #[serde(default = "d_read_user_profile")]
    pub read_user_profile: String,
    #[serde(default = "d_send_message")]
    pub send_message: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitHub {
    #[serde(default = "d_hosts")]
    pub hosts: Vec<String>,
    #[serde(default = "d_api_url")]
    pub api_url: String,
    /// Where the token comes from. Never the token itself — this file is
    /// meant to be committable.
    #[serde(default)]
    pub token_env: Option<String>,
    /// Defaults to `gh auth token`, which is already set up on most machines
    /// that would want this.
    #[serde(default = "d_token_command")]
    pub token_command: Option<String>,
    /// Checks that say nothing about the change — a merge lock, a label
    /// policy — by exact name. They never turn a pull request red.
    #[serde(default)]
    pub ignore_checks: Vec<String>,
}

/// The resident server: how often it looks, and when it gives up.
///
/// Nobody starts it — `inkling subscribe` and `inkling watch` do, when there
/// is something to serve — so these are the only knobs it has.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Serve {
    /// How often it polls and delivers. Short, because CI moving is the news
    /// it exists to carry and a minute of it is a minute of a session
    /// waiting.
    #[serde(with = "humantime_serde", default = "d_serve_interval")]
    pub interval: Duration,
    /// How long with nothing to do before standing down. Generous on
    /// purpose: a session that detaches and reattaches shouldn't cost a
    /// respawn.
    #[serde(with = "humantime_serde", default = "d_serve_idle_after")]
    pub idle_after: Duration,
    /// A cap, so a resident process eventually picks up a newer build.
    #[serde(with = "humantime_serde", default = "d_serve_max_lifetime")]
    pub max_lifetime: Duration,
}

impl Config {
    /// Load from disk, or fall back to defaults when there's no file.
    ///
    /// A config file is a way to change Inkling's mind, not a prerequisite for
    /// running it — every field has a working default.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(raw) => Self::parse(&raw, path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::with_defaults()),
            Err(source) => Err(ConfigError::Read {
                path: path.to_owned(),
                source,
            }),
        }
    }

    /// What an empty config file means.
    pub fn with_defaults() -> Self {
        Self::parse("", "<defaults>").expect("defaults must be valid")
    }

    pub fn parse(raw: &str, path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let parse_error = |mut source: toml::de::Error| {
            // Deserializing from a table rather than the text drops the
            // input the error quotes; its span survives.
            source.set_input(Some(raw));
            ConfigError::Parse {
                path: path.as_ref().to_owned(),
                source,
            }
        };
        let mut root = toml::de::DeTable::parse(raw).map_err(parse_error)?;
        for key in RETIRED {
            strip(root.get_mut(), key);
        }
        let config =
            Config::deserialize(toml::de::Deserializer::from(root)).map_err(parse_error)?;
        config.validate()?;
        Ok(config)
    }

    /// What would otherwise fail later, out of sight in a hook or the server.
    fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |msg: String| ConfigError::Invalid(msg);

        if self.github.hosts.is_empty() {
            return Err(invalid("github.hosts is empty; nothing would match".into()));
        }
        // Every reference is read from api_url, so a second host's pull
        // request would be read as the same number on the first host.
        if self.github.hosts.len() > 1 {
            return Err(invalid(format!(
                "github.hosts lists {}, but every host is read from one api_url ({}); \
                 list only the host api_url serves",
                self.github.hosts.join(", "),
                self.github.api_url
            )));
        }

        Ok(())
    }
}

fn d_hosts() -> Vec<String> {
    vec!["github.com".to_string()]
}
fn d_token_command() -> Option<String> {
    Some("gh auth token".to_string())
}

// Spelled out rather than derived from `slack_<role>`: react doesn't follow it.
fn d_read_channel() -> String {
    "slack_read_channel".into()
}
fn d_react() -> String {
    "slack_add_reaction".into()
}
fn d_search_channels() -> String {
    "slack_search_channels".into()
}
fn d_read_thread() -> String {
    "slack_read_thread".into()
}
fn d_read_user_profile() -> String {
    "slack_read_user_profile".into()
}
fn d_send_message() -> String {
    "slack_send_message".into()
}

impl Default for SlackMcpTools {
    fn default() -> Self {
        Self {
            read_channel: d_read_channel(),
            react: d_react(),
            search_channels: d_search_channels(),
            read_thread: d_read_thread(),
            read_user_profile: d_read_user_profile(),
            send_message: d_send_message(),
        }
    }
}

impl SlackMcp {
    /// The `--mcp-config` payload naming only this server.
    ///
    /// Header values are expanded from the environment here rather than at
    /// load time, so a `Config` never holds a secret in memory longer than
    /// the call that needs it.
    pub fn config_json(&self) -> Result<String, ConfigError> {
        let mut headers = serde_json::Map::new();
        for (name, value) in &self.headers {
            headers.insert(name.clone(), expand_env(value)?.into());
        }
        let server = serde_json::json!({
            "type": "http",
            "url": self.url,
            "headers": headers,
        });
        serde_json::to_string(&serde_json::json!({
            "mcpServers": { self.server.clone(): server }
        }))
        .map_err(|e| ConfigError::Invalid(format!("building mcp config: {e}")))
    }
}

/// Replace `${VAR}` with the environment's value.
fn expand_env(raw: &str) -> Result<String, ConfigError> {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            // An unterminated ${ is a typo, not a literal.
            return Err(ConfigError::Invalid(format!(
                "unterminated ${{ in header value: {raw}"
            )));
        };
        let name = &after[..end];
        let value = std::env::var(name).map_err(|_| {
            ConfigError::Invalid(format!("header references ${{{name}}}, which is not set"))
        })?;
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn d_api_url() -> String {
    "https://api.github.com/graphql".to_string()
}
fn d_serve_interval() -> Duration {
    Duration::from_secs(15)
}
fn d_serve_idle_after() -> Duration {
    Duration::from_secs(300)
}
fn d_serve_max_lifetime() -> Duration {
    Duration::from_secs(60 * 60 * 24)
}

impl Default for Serve {
    fn default() -> Self {
        Self {
            interval: d_serve_interval(),
            idle_after: d_serve_idle_after(),
            max_lifetime: d_serve_max_lifetime(),
        }
    }
}

impl Default for GitHub {
    fn default() -> Self {
        Self {
            hosts: d_hosts(),
            api_url: d_api_url(),
            token_env: None,
            token_command: d_token_command(),
            ignore_checks: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r##"
        [slack]
        user_id = "U012ABC345"

        [serve]
        interval = "30s"
    "##;

    fn parse(raw: &str) -> Result<Config, ConfigError> {
        Config::parse(raw, "test.toml")
    }

    fn error(raw: &str) -> String {
        parse(raw).expect_err("should be rejected").to_string()
    }

    #[test]
    fn an_empty_config_is_a_working_config() {
        // A config file changes Inkling's mind; it isn't a prerequisite for
        // running.
        let config = Config::with_defaults();
        assert!(config.github.token_command.is_some());
        assert_eq!(config.github.hosts, vec!["github.com"]);
        assert_eq!(config.serve.interval, Duration::from_secs(15));
    }

    #[test]
    fn a_derived_default_cannot_drift_from_the_serde_defaults() {
        // `#[serde(default)]` on the containing field builds the struct via
        // Default, so the two must agree or an omitted section silently
        // means something different from an empty one.
        let from_serde = parse("[github]\n[serve]\n").unwrap();
        assert_eq!(
            from_serde.github.token_command,
            GitHub::default().token_command
        );
        assert_eq!(from_serde.serve.idle_after, Serve::default().idle_after);
    }

    #[test]
    fn a_config_written_for_the_removed_reactions_still_loads() {
        // Their sections and keys are ignored; nothing else about the file
        // changes.
        let raw = r##"
            [window]
            overlap = "10m"

            [tracking]
            doubling_period = "0s"

            [filters]
            only_my_messages = false

            [run]
            dry_run = true

            [status.merged]
            canonical = "rocket"

            [slack]
            personal = false
            channels = ["#eng"]
            user_id = "U012ABC345"

            [slack.mcp]
            server = "s"
            url = "u"

            [slack.mcp.tools]
            search = "slack_search_public_and_private"
            read_thread = "read_thread"
        "##;
        let config = parse(raw).unwrap();
        assert_eq!(config.slack.user_id.as_deref(), Some("U012ABC345"));
        assert_eq!(config.slack.mcp.unwrap().tools.read_thread, "read_thread");
    }

    #[test]
    fn checks_to_ignore_are_named_under_github() {
        let config = parse(&format!(
            "{EXAMPLE}\n[github]\nignore_checks = [\"merge-lock\"]\n"
        ))
        .unwrap();
        assert_eq!(config.github.ignore_checks, vec!["merge-lock"]);
        assert!(parse(EXAMPLE).unwrap().github.ignore_checks.is_empty());
    }

    #[test]
    fn durations_are_human_readable() {
        assert_eq!(
            parse(EXAMPLE).unwrap().serve.interval,
            Duration::from_secs(30)
        );
    }

    #[test]
    fn an_omitted_mcp_table_changes_nothing() {
        // The whole feature is additive: without it the bridge keeps
        // inheriting the ambient MCP configuration exactly as before.
        assert_eq!(parse(EXAMPLE).unwrap().slack.mcp, None);
    }

    #[test]
    fn an_mcp_table_defaults_every_tool_name() {
        let raw =
            format!("{EXAMPLE}\n[slack.mcp]\nserver = \"my-slack\"\nurl = \"https://x/mcp\"\n");
        let mcp = parse(&raw).unwrap().slack.mcp.unwrap();
        assert_eq!(mcp.server, "my-slack");
        assert_eq!(mcp.tools, SlackMcpTools::default());
        assert_eq!(mcp.tools.react, "slack_add_reaction");
    }

    #[test]
    fn tool_names_can_be_overridden_one_at_a_time() {
        let raw = format!(
            "{EXAMPLE}\n[slack.mcp]\nserver = \"s\"\nurl = \"u\"\n\
             [slack.mcp.tools]\nreact = \"add_reaction\"\n"
        );
        let mcp = parse(&raw).unwrap().slack.mcp.unwrap();
        assert_eq!(mcp.tools.react, "add_reaction");
        assert_eq!(
            mcp.tools.read_thread,
            d_read_thread(),
            "the rest keep their defaults"
        );
    }

    #[test]
    fn an_unknown_mcp_key_is_rejected() {
        let raw = format!("{EXAMPLE}\n[slack.mcp]\nserver = \"s\"\nurl = \"u\"\nnope = 1\n");
        assert!(error(&raw).contains("parsing"));
    }

    #[test]
    fn headers_expand_from_the_environment() {
        // A literal secret in a committed config is exactly what this avoids.
        unsafe { std::env::set_var("INKLING_TEST_MCP_KEY", "s3cret") };
        let raw = format!(
            "{EXAMPLE}\n[slack.mcp]\nserver = \"s\"\nurl = \"https://x/mcp\"\n\
             headers = {{ \"x-api-key\" = \"${{INKLING_TEST_MCP_KEY}}\" }}\n"
        );
        let json = parse(&raw)
            .unwrap()
            .slack
            .mcp
            .unwrap()
            .config_json()
            .unwrap();
        assert!(json.contains(r#""x-api-key":"s3cret""#), "{json}");
        assert!(json.contains(r#""mcpServers":{"s""#), "{json}");
    }

    #[test]
    fn a_missing_environment_variable_is_named_rather_than_left_empty() {
        let raw = format!(
            "{EXAMPLE}\n[slack.mcp]\nserver = \"s\"\nurl = \"u\"\n\
             headers = {{ \"k\" = \"${{INKLING_DEFINITELY_UNSET}}\" }}\n"
        );
        let error = parse(&raw)
            .unwrap()
            .slack
            .mcp
            .unwrap()
            .config_json()
            .unwrap_err();
        assert!(error.to_string().contains("INKLING_DEFINITELY_UNSET"));
    }

    #[test]
    fn an_unknown_key_is_rejected_rather_than_ignored() {
        // A typo would otherwise silently leave the default in place.
        let err = error(&format!("{EXAMPLE}\nidle_aftr = \"1m\"\n"));
        assert!(err.contains("parsing"), "{err}");
        let err = error("[watch]\nttl = \"1d\"\n");
        assert!(err.contains("parsing"), "{err}");
    }

    #[test]
    fn an_unknown_key_names_only_live_sections_and_where_it_is() {
        // Retired sections load, but aren't keys anyone should be pointed at.
        let err = error("[serve]\ninterval = \"5s\"\n\n[bogus]\nkey = 1\n");
        assert!(err.contains("`slack`, `github`, `serve`"), "{err}");
        assert!(!err.contains("window"), "{err}");
        assert!(err.contains("line 4"), "{err}");
    }

    #[test]
    fn an_omitted_user_id_is_fine_because_it_gets_resolved() {
        assert_eq!(Config::with_defaults().slack.user_id, None);
    }

    #[test]
    fn a_second_github_host_is_rejected_rather_than_read_from_the_first_ones_api() {
        let raw = format!("{EXAMPLE}\n[github]\nhosts = [\"github.com\", \"ghe.example.com\"]\n");
        assert!(error(&raw).contains("one api_url"), "{}", error(&raw));

        let enterprise = format!(
            "{EXAMPLE}\n[github]\nhosts = [\"ghe.example.com\"]\napi_url = \"https://ghe.example.com/api/graphql\"\n"
        );
        assert!(parse(&enterprise).is_ok());
    }
}
