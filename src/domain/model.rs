//! The vocabulary everything else is expressed in: the things a watch points
//! at, and how far along they are.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// A Slack emoji name, normalized. Slack reports reactions without colons but
/// humans write them with, and casing varies; comparing raw strings would let
/// `:LGTM:` and `lgtm` read as different signals.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ReactionName(String);

impl ReactionName {
    pub fn new(raw: impl AsRef<str>) -> Self {
        Self(raw.as_ref().trim().trim_matches(':').to_lowercase())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ReactionName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A recognized pointer to something worth watching.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Reference {
    GitHubPullRequest {
        host: String,
        owner: String,
        repo: String,
        number: u64,
    },
    /// No site: `acli` is authenticated to exactly one, and carrying a field
    /// nothing reads would imply a multi-site story that doesn't exist.
    JiraIssue { key: String },
    /// One Slack message, watched for replies and reactions. `ts` is the
    /// message's own; `thread` is the root it replies to, when it is a reply —
    /// replies are read from the root, so a reply alone can't find them.
    SlackMessage {
        channel: String,
        ts: String,
        thread: Option<String>,
    },
    /// A whole Slack conversation, watched for new top-level messages. `name`
    /// is display only (`#incidents`), filled in when it is known; the id is
    /// what is read.
    SlackChannel {
        channel: String,
        name: Option<String>,
    },
}

impl Reference {
    /// A pull request, lowercased: GitHub ignores case in all three, and the
    /// key is case-sensitive, so `CLI/Cli#1` and `cli/cli#1` must spell alike.
    pub fn pull_request(host: &str, owner: &str, repo: &str, number: u64) -> Self {
        Reference::GitHubPullRequest {
            host: host.to_lowercase(),
            owner: owner.to_lowercase(),
            repo: repo.to_lowercase(),
            number,
        }
    }

    /// What makes two references one watch: the display form, except that a
    /// channel is its id whether or not its name is known yet.
    pub fn key(&self) -> String {
        match self {
            Reference::SlackChannel { channel, .. } => format!("slack:{channel}"),
            other => other.to_string(),
        }
    }
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reference::GitHubPullRequest {
                host,
                owner,
                repo,
                number,
            } => write!(f, "{host}/{owner}/{repo}#{number}"),
            Reference::JiraIssue { key } => f.write_str(key),
            // Channel and ts alone: they name the message, and they are what
            // an agent hands Slack's own tools to read it. The thread root is
            // how to read it, not which message it is.
            Reference::SlackMessage { channel, ts, .. } => write!(f, "slack:{channel}/{ts}"),
            Reference::SlackChannel {
                name: Some(name), ..
            } => write!(f, "slack:#{name}"),
            Reference::SlackChannel { channel, .. } => write!(f, "slack:{channel}"),
        }
    }
}

/// How far along the lifecycle something is.
///
/// The declaration order *is* the ordering, and the ordering is the whole
/// mechanism: aggregating a message's references takes the minimum, and the
/// signal drawn is the one for that state. `Closed` sits lowest because a
/// closed PR is going nowhere — which is what lets a message holding one
/// approved and one closed PR say nothing at all, rather than claiming the
/// approval and ignoring the corpse.
///
/// There is no transition graph. GitHub is the authority on what state a PR
/// is in; nothing here could meaningfully disbelieve it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Closed,
    Draft,
    Open,
    Approved,
    Merged,
}

impl State {
    /// Nothing follows a terminal state, so a reference in one never needs
    /// checking again. This is a correctness property — it decides when to
    /// stop polling — and is deliberately separate from whether config draws
    /// a signal for the state.
    pub fn is_terminal(self) -> bool {
        matches!(self, State::Merged | State::Closed)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            State::Closed => "closed",
            State::Draft => "draft",
            State::Open => "open",
            State::Approved => "approved",
            State::Merged => "merged",
        }
    }

    pub const ALL: [State; 5] = [
        State::Closed,
        State::Draft,
        State::Open,
        State::Approved,
        State::Merged,
    ];
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // pad, not write_str: write_str ignores width, so `{state:<10}` in a
        // aligned column would silently do nothing.
        f.pad(self.as_str())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("unknown state: {0}")]
pub struct UnknownState(pub String);

impl FromStr for State {
    type Err = UnknownState;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        State::ALL
            .into_iter()
            .find(|state| state.as_str() == s)
            .ok_or_else(|| UnknownState(s.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_are_ordered_by_progress_toward_merged() {
        assert!(State::Open < State::Approved);
        assert!(State::Approved < State::Merged);
        assert!(State::Draft < State::Open);
    }

    #[test]
    fn closed_ranks_below_every_live_state() {
        // A message holding one approved and one closed PR aggregates to
        // Closed, which draws nothing — the point of putting it lowest.
        for state in [State::Draft, State::Open, State::Approved, State::Merged] {
            assert!(State::Closed < state, "Closed should rank below {state}");
        }
    }

    #[test]
    fn merged_and_closed_are_the_terminal_states() {
        let terminal: Vec<_> = State::ALL.into_iter().filter(|s| s.is_terminal()).collect();
        assert_eq!(terminal, vec![State::Closed, State::Merged]);
    }

    #[test]
    fn state_names_round_trip_through_config_keys() {
        for state in State::ALL {
            assert_eq!(state.as_str().parse(), Ok(state));
        }
    }

    #[test]
    fn unknown_state_names_are_rejected() {
        assert!("changes_requested".parse::<State>().is_err());
        assert!("".parse::<State>().is_err());
    }

    #[test]
    fn reaction_names_normalize_colons_and_case() {
        assert_eq!(ReactionName::new(":LGTM:"), ReactionName::new("lgtm"));
        assert_eq!(ReactionName::new("  eyes  "), ReactionName::new("eyes"));
        assert_eq!(ReactionName::new(":shipit:").as_str(), "shipit");
    }

    #[test]
    fn a_slack_message_is_named_by_channel_and_ts_alone() {
        let reply = Reference::SlackMessage {
            channel: "C0123ABC".into(),
            ts: "1786596050.245879".into(),
            thread: Some("1786596000.123456".into()),
        };
        assert_eq!(reply.to_string(), "slack:C0123ABC/1786596050.245879");
    }

    #[test]
    fn a_slack_channel_is_named_by_its_handle_when_known() {
        let channel = |name: Option<&str>| Reference::SlackChannel {
            channel: "C0E48AHSN".into(),
            name: name.map(Into::into),
        };
        assert_eq!(channel(Some("general")).to_string(), "slack:#general");
        assert_eq!(channel(None).to_string(), "slack:C0E48AHSN");
    }
}
