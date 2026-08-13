//! The core. Pure functions over data — no I/O, and no knowledge of Slack,
//! GitHub, or Claude. Nothing in here may import from `adapters`.

pub mod detect;
pub mod model;
pub mod reconcile;
pub mod schedule;
pub mod window;
