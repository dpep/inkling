//! Configuration: what to watch, how often to look, and which emoji mean what.
//!
//! Emoji vocabulary differs from org to org — one team's merged is `:merged:`,
//! another's is `:shipit:` — so the whole mapping lives here and none of it
//! leaks into the core. A state is signaled only if it appears in `[status.*]`
//! and isn't disabled, which is also how states stay recognized-but-silent.

use crate::domain::model::{ReactionName, State};
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
    pub window: Window,
    #[serde(default)]
    pub tracking: Tracking,
    #[serde(default)]
    pub filters: Filters,
    #[serde(default)]
    pub slack: Slack,
    #[serde(default)]
    pub github: GitHub,
    /// Keyed by state name. A state absent here is never drawn.
    #[serde(default)]
    pub status: BTreeMap<String, Status>,
    #[serde(default)]
    pub run: Run,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    /// Re-scan this far back past the last successful discovery, so a message
    /// landing right on the boundary isn't missed.
    #[serde(with = "humantime_serde", default = "d_overlap")]
    pub overlap: Duration,
    /// Hard clamp. A corrupt or ancient timestamp can never widen the search
    /// past this.
    #[serde(with = "humantime_serde", default = "d_max_lookback")]
    pub max_lookback: Duration,
    /// First run, or after state loss.
    #[serde(with = "humantime_serde", default = "d_initial_lookback")]
    pub initial_lookback: Duration,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tracking {
    /// How long a message stays worth updating. Short on purpose: nobody
    /// scrolls back to a week-old post to see whether the PR landed.
    #[serde(with = "humantime_serde", default = "d_max_tracking_age")]
    pub max_tracking_age: Duration,
    /// Interval between checks while the message is fresh. Defaults to the
    /// expected cron cadence, so a fresh message is effectively checked every
    /// run. Zero disables backoff entirely.
    #[serde(with = "humantime_serde", default = "d_base_check_interval")]
    pub base_check_interval: Duration,
    /// The check interval doubles once per this much message age.
    #[serde(with = "humantime_serde", default = "d_doubling_period")]
    pub doubling_period: Duration,
    #[serde(with = "humantime_serde", default = "d_max_check_interval")]
    pub max_check_interval: Duration,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Filters {
    /// A long message is a digest or a standup post; a reaction on it says
    /// nothing useful.
    #[serde(default = "d_max_message_chars")]
    pub max_message_chars: usize,
    #[serde(default = "d_max_references")]
    pub max_references_per_message: usize,
    /// Annotate only messages the configured user wrote, however they were
    /// discovered. On by default — your own posts are the safe blast radius.
    #[serde(default = "d_true")]
    pub only_my_messages: bool,
}

// Default is implemented by hand rather than derived: `#[serde(default)]` on
// the containing field constructs the whole struct via Default, bypassing the
// per-field defaults below. Deriving it silently reverts them to false/None.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Slack {
    /// Search for messages the configured user authored. On by default, so
    /// there's always something to search without any configuration.
    #[serde(default = "d_true")]
    pub personal: bool,
    /// Slack user ID, e.g. `U012ABC345`. Needed by personal mode and by
    /// `only_my_messages`, but resolved from the credentials and cached when
    /// omitted — nobody knows their own Slack ID off-hand.
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub channels: Vec<String>,
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
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
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
/// backend that matches needs none of this.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SlackMcpTools {
    #[serde(default = "d_search")]
    pub search: String,
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
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Status {
    /// What Inkling adds when it needs to signal this state.
    pub canonical: String,
    /// Other emoji that count as evidence this state is already signaled,
    /// from anyone. Inkling reads these but only ever writes `canonical`.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Keep the mapping, stop drawing it.
    #[serde(default = "d_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Run {
    #[serde(default)]
    pub dry_run: bool,
}

impl Config {
    /// Load from disk, or fall back to defaults when there's no file.
    ///
    /// A config file is a way to change Inkling's mind, not a prerequisite for
    /// running it — every field has a working default, so a fresh machine can
    /// go straight to `inkling run --dry-run`.
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
        let mut config: Config = toml::from_str(raw).map_err(|source| ConfigError::Parse {
            path: path.as_ref().to_owned(),
            source,
        })?;
        // Only when nothing was configured at all — a file that names one
        // state means the others were left out deliberately.
        if config.status.is_empty() {
            config.status = default_status();
        }
        config.validate()?;
        Ok(config)
    }

    /// Everything that would otherwise fail at 3am from cron: an unknown state
    /// name, an emoji claimed by two states, a mode with nothing to search.
    fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |msg: String| ConfigError::Invalid(msg);

        let mut owner: BTreeMap<ReactionName, String> = BTreeMap::new();
        for (name, status) in &self.status {
            name.parse::<State>()
                .map_err(|_| invalid(format!("[status.{name}]: unknown state")))?;

            if status.canonical.trim().is_empty() {
                return Err(invalid(format!("[status.{name}]: canonical is empty")));
            }

            for emoji in std::iter::once(&status.canonical).chain(&status.aliases) {
                let emoji = ReactionName::new(emoji);
                if let Some(other) = owner.insert(emoji.clone(), name.clone())
                    && &other != name
                {
                    return Err(invalid(format!(
                        ":{emoji}: is mapped to both {other} and {name}; \
                         one emoji can only mean one thing"
                    )));
                }
            }
        }

        if !self.slack.personal && self.slack.channels.is_empty() {
            return Err(invalid(
                "nothing to search: enable slack.personal or list slack.channels".into(),
            ));
        }

        // slack.user_id is deliberately not required: the modes that need it
        // resolve it from the credentials at run time and cache it.

        if self.github.hosts.is_empty() {
            return Err(invalid("github.hosts is empty; nothing would match".into()));
        }

        if self.tracking.doubling_period.is_zero() {
            return Err(invalid(
                "tracking.doubling_period must be non-zero; it divides message age".into(),
            ));
        }

        Ok(())
    }

    /// The emoji to add when signaling `state`, or `None` if config doesn't
    /// draw it.
    pub fn canonical_for(&self, state: State) -> Option<ReactionName> {
        let status = self.status.get(state.as_str())?;
        status.enabled.then(|| ReactionName::new(&status.canonical))
    }

    /// Every emoji that counts as evidence `state` is already signaled.
    /// Includes disabled states: a signal someone else left still means
    /// something even if Inkling wouldn't have drawn it.
    pub fn evidence_for(&self, state: State) -> Vec<ReactionName> {
        let Some(status) = self.status.get(state.as_str()) else {
            return Vec::new();
        };
        std::iter::once(&status.canonical)
            .chain(&status.aliases)
            .map(ReactionName::new)
            .collect()
    }
}

fn d_true() -> bool {
    true
}
fn d_overlap() -> Duration {
    Duration::from_secs(10 * 60)
}
fn d_max_lookback() -> Duration {
    Duration::from_secs(24 * 60 * 60)
}
fn d_initial_lookback() -> Duration {
    Duration::from_secs(2 * 60 * 60)
}
fn d_max_tracking_age() -> Duration {
    Duration::from_secs(7 * 24 * 60 * 60)
}
fn d_base_check_interval() -> Duration {
    Duration::from_secs(20 * 60)
}
fn d_doubling_period() -> Duration {
    Duration::from_secs(24 * 60 * 60)
}
fn d_max_check_interval() -> Duration {
    Duration::from_secs(24 * 60 * 60)
}
fn d_max_message_chars() -> usize {
    1000
}
fn d_max_references() -> usize {
    3
}
fn d_hosts() -> Vec<String> {
    vec!["github.com".to_string()]
}
fn d_token_command() -> Option<String> {
    Some("gh auth token".to_string())
}

// Most roles follow `slack_<role>`; search and react don't, so every default
// is spelled out rather than derived from a pattern that holds five times
// out of seven.
fn d_search() -> String {
    "slack_search_public_and_private".into()
}
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
            search: d_search(),
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

/// Emoji every workspace has. `:merged:` and `:shipit:` are common but not
/// universal, and adding one the workspace lacks fails with invalid_name —
/// so they're accepted as evidence, never written.
fn default_status() -> BTreeMap<String, Status> {
    BTreeMap::from([
        (
            State::Approved.as_str().to_string(),
            Status {
                canonical: "white_check_mark".into(),
                aliases: ["heavy_check_mark", "done", "lgtm", "eyes"]
                    .map(String::from)
                    .into(),
                enabled: true,
            },
        ),
        (
            State::Merged.as_str().to_string(),
            Status {
                canonical: "purple_heart".into(),
                aliases: ["merged", "shipit"].map(String::from).into(),
                enabled: true,
            },
        ),
    ])
}
fn d_api_url() -> String {
    "https://api.github.com/graphql".to_string()
}

impl Default for Window {
    fn default() -> Self {
        Self {
            overlap: d_overlap(),
            max_lookback: d_max_lookback(),
            initial_lookback: d_initial_lookback(),
        }
    }
}

impl Default for Tracking {
    fn default() -> Self {
        Self {
            max_tracking_age: d_max_tracking_age(),
            base_check_interval: d_base_check_interval(),
            doubling_period: d_doubling_period(),
            max_check_interval: d_max_check_interval(),
        }
    }
}

impl Default for Filters {
    fn default() -> Self {
        Self {
            max_message_chars: d_max_message_chars(),
            max_references_per_message: d_max_references(),
            only_my_messages: d_true(),
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
        }
    }
}

impl Default for Slack {
    fn default() -> Self {
        Self {
            personal: d_true(),
            user_id: None,
            channels: Vec::new(),
            mcp: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r##"
        [slack]
        personal = true
        user_id = "U012ABC345"
        channels = ["#coding"]

        [status.approved]
        canonical = "white_check_mark"
        aliases = ["heavy_check_mark", "lgtm"]

        [status.merged]
        canonical = "merged"
        aliases = ["shipit"]
    "##;

    fn parse(raw: &str) -> Result<Config, ConfigError> {
        Config::parse(raw, "test.toml")
    }

    fn example() -> Config {
        parse(EXAMPLE).expect("example config should be valid")
    }

    fn error(raw: &str) -> String {
        parse(raw).expect_err("should be rejected").to_string()
    }

    #[test]
    fn an_empty_config_is_a_working_config() {
        // A config file changes Inkling's mind; it isn't a prerequisite for
        // running. A fresh machine should go straight to a dry run.
        let config = Config::with_defaults();
        assert!(config.slack.personal, "something must be searchable");
        assert!(config.filters.only_my_messages);
        assert_eq!(
            config.canonical_for(State::Merged),
            Some(ReactionName::new("purple_heart"))
        );
        assert!(config.github.token_command.is_some());
    }

    #[test]
    fn a_derived_default_cannot_drift_from_the_serde_defaults() {
        // `#[serde(default)]` on the containing field builds the struct via
        // Default, so the two must agree or an omitted section silently
        // means something different from an empty one.
        let from_serde = parse("[status.merged]\ncanonical = \"x\"\n").unwrap();
        assert_eq!(from_serde.slack.personal, Slack::default().personal);
        assert_eq!(
            from_serde.github.token_command,
            GitHub::default().token_command
        );
    }

    #[test]
    fn omitted_sections_fall_back_to_defaults() {
        let config = example();
        assert_eq!(config.window.overlap, Duration::from_secs(600));
        assert_eq!(
            config.tracking.max_tracking_age,
            Duration::from_secs(7 * 24 * 3600)
        );
        assert_eq!(config.filters.max_message_chars, 1000);
        assert!(config.filters.only_my_messages);
        assert_eq!(config.github.hosts, vec!["github.com"]);
    }

    #[test]
    fn durations_are_human_readable() {
        let config = parse(&format!("{EXAMPLE}\n[window]\noverlap = \"90s\"\n")).unwrap();
        assert_eq!(config.window.overlap, Duration::from_secs(90));
    }

    #[test]
    fn canonical_emoji_comes_from_config() {
        let config = example();
        assert_eq!(
            config.canonical_for(State::Merged),
            Some(ReactionName::new("merged"))
        );
        // Not configured, so never drawn — even though it's a real state.
        assert_eq!(config.canonical_for(State::Open), None);
    }

    #[test]
    fn a_disabled_state_is_not_drawn_but_still_counts_as_evidence() {
        let raw =
            format!("{EXAMPLE}\n[status.closed]\ncanonical = \"wastebasket\"\nenabled = false\n");
        let config = parse(&raw).unwrap();
        assert_eq!(config.canonical_for(State::Closed), None);
        assert_eq!(
            config.evidence_for(State::Closed),
            vec![ReactionName::new("wastebasket")]
        );
    }

    #[test]
    fn evidence_covers_canonical_and_aliases() {
        assert_eq!(
            example().evidence_for(State::Approved),
            vec![
                ReactionName::new("white_check_mark"),
                ReactionName::new("heavy_check_mark"),
                ReactionName::new("lgtm"),
            ]
        );
    }

    #[test]
    fn an_omitted_mcp_table_changes_nothing() {
        // The whole feature is additive: without it the bridge keeps
        // inheriting the ambient MCP configuration exactly as before.
        assert_eq!(example().slack.mcp, None);
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
        assert_eq!(mcp.tools.search, d_search(), "the rest keep their defaults");
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
        // A typo in an emoji mapping would otherwise silently no-op.
        let err = error(&format!("{EXAMPLE}\n[filters]\nmax_message_charz = 10\n"));
        assert!(err.contains("parsing"), "{err}");
    }

    #[test]
    fn an_unknown_state_name_is_rejected() {
        let raw = format!("{EXAMPLE}\n[status.changes_requested]\ncanonical = \"eyes\"\n");
        assert!(error(&raw).contains("unknown state"));
    }

    #[test]
    fn one_emoji_cannot_mean_two_things() {
        let raw = r#"
            [slack]
            personal = true
            user_id = "U1"

            [status.approved]
            canonical = "eyes"

            [status.merged]
            canonical = "merged"
            aliases = ["eyes"]
        "#;
        assert!(error(raw).contains("only mean one thing"));
    }

    #[test]
    fn repeating_an_emoji_within_one_state_is_fine() {
        let raw = r#"
            [slack]
            personal = true
            user_id = "U1"

            [status.approved]
            canonical = "eyes"
            aliases = [":EYES:"]
        "#;
        assert!(parse(raw).is_ok());
    }

    #[test]
    fn a_run_with_nothing_to_search_is_rejected() {
        let raw = r#"
            [slack]
            personal = false

            [status.merged]
            canonical = "merged"
        "#;
        assert!(error(raw).contains("nothing to search"));
    }

    #[test]
    fn an_omitted_user_id_is_fine_because_it_gets_resolved() {
        let raw = r#"
            [slack]
            personal = true

            [status.merged]
            canonical = "merged"
        "#;
        let config = parse(raw).unwrap();
        assert_eq!(config.slack.user_id, None);
    }

    #[test]
    fn a_zero_doubling_period_is_rejected() {
        let raw = format!("{EXAMPLE}\n[tracking]\ndoubling_period = \"0s\"\n");
        assert!(error(&raw).contains("doubling_period"));
    }

    #[test]
    fn an_empty_canonical_is_rejected() {
        let raw = r#"
            [slack]
            personal = true
            user_id = "U1"

            [status.merged]
            canonical = ""
        "#;
        assert!(error(raw).contains("canonical is empty"));
    }
}
