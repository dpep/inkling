//! Inkling tells an agent when a pull request, Jira issue or Slack message it
//! cares about changes.
//!
//! This library is the `inkling` binary's internals, not a published API: it
//! carries no semver guarantee. See `docs/` for the design reasoning.

pub mod adapters;
pub mod config;
pub mod config_edit;
pub mod domain;
pub mod obs;
pub mod ports;
pub mod push;
pub mod secret;
pub mod serve;
pub mod subprocess;
#[cfg(test)]
mod testing;
pub mod wait;
pub mod watch;
