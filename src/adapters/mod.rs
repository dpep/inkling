//! Everything that talks to the outside world. The domain knows none of it.

pub mod github;
pub mod jira_acli;
pub mod lock;
pub mod slack_claude;
pub mod store_json;
pub mod waits_json;
pub mod watches_json;
