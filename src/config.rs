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
    /// Interval between checks while the message is fresh. Zero = every run.
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

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Slack {
    /// Search for messages the configured user authored.
    #[serde(default)]
    pub personal: bool,
    /// Slack user ID, e.g. `U0E48AHQA`. Required by personal mode and by
    /// `only_my_messages`.
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub channels: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitHub {
    #[serde(default = "d_hosts")]
    pub hosts: Vec<String>,
    /// Where the token comes from. Never the token itself — this file is
    /// meant to be committable.
    #[serde(default)]
    pub token_env: Option<String>,
    #[serde(default)]
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
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(&raw, path)
    }

    pub fn parse(raw: &str, path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let config: Config = toml::from_str(raw).map_err(|source| ConfigError::Parse {
            path: path.as_ref().to_owned(),
            source,
        })?;
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

        let needs_user_id = self.slack.personal || self.filters.only_my_messages;
        if needs_user_id && self.slack.user_id.is_none() {
            return Err(invalid(
                "slack.user_id is required by slack.personal and filters.only_my_messages".into(),
            ));
        }

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
    Duration::ZERO
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
            token_env: None,
            token_command: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r##"
        [slack]
        personal = true
        user_id = "U0E48AHQA"
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
    fn modes_that_need_a_user_id_say_so() {
        let raw = r#"
            [slack]
            personal = true

            [status.merged]
            canonical = "merged"
        "#;
        assert!(error(raw).contains("slack.user_id is required"));
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
