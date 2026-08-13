//! Inkling turns the current state of things referenced in Slack into small,
//! persistent signals on the messages that referenced them.
//!
//! See `DESIGN.md` for the architecture and `PLAN.md` for what is built when.

pub mod adapters;
pub mod config;
pub mod domain;
pub mod ports;
pub mod secret;
#[cfg(any(test, feature = "test-support"))]
pub mod testing;
