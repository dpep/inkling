//! Jira issue status, through the `acli` binary.
//!
//! Shelling out rather than calling the REST API directly, because `acli` is
//! already installed and already authenticated. The alternative is asking the
//! user for an API token, storing it, and refreshing it — a credential this
//! tool would then own and have to protect, for a read it can get for free.
//! The cost is a subprocess per issue and a dependency on a binary being on
//! `PATH`; both are cheap at the volume a watch list runs at.
//!
//! `acli jira workitem view --json` returns the raw REST issue resource, so
//! the parsing here is against Jira's contract rather than against `acli`'s.
//! Only the shelling out would change if this ever moved to REST.

use crate::domain::watch::Category;
use crate::ports::{IssueLookup, IssueProvider, IssueStatus, ProviderError, RefError};
use serde::Deserialize;
use std::process::Command;

/// One `acli` invocation. Behind a trait so tests replay recorded output
/// instead of needing a Jira site.
pub trait Shell {
    /// Stdout on success; stderr on failure, which is where `acli` puts the
    /// reason and where it exits 1 (recorded 2026-09-20, acli 1.3.19).
    fn view(&self, key: &str) -> Result<String, Failure>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// `acli` ran and said no.
    Refused(String),
    /// `acli` didn't run at all — not installed, not on PATH.
    Unavailable(String),
}

pub struct AcliShell {
    pub command: String,
}

impl Default for AcliShell {
    fn default() -> Self {
        Self {
            command: "acli".into(),
        }
    }
}

impl AcliShell {
    /// Naming the fields keeps the payload to what is read — the default set
    /// includes a description in Atlassian Document Format, a large nested
    /// blob nothing here looks at.
    fn args(key: &str) -> [&str; 7] {
        [
            "jira",
            "workitem",
            "view",
            key,
            "--json",
            "--fields",
            "key,summary,status",
        ]
    }
}

/// How long one issue gets. Measured against this machine's `acli`: 1.5s for
/// the answer, authenticated or not. A read that takes longer than this has
/// stopped being a read — and a poll does one of these per watched issue, so
/// an unbounded one is an unbounded tick.
const VIEW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

impl Shell for AcliShell {
    fn view(&self, key: &str) -> Result<String, Failure> {
        let output = crate::subprocess::output_within(
            Command::new(&self.command).args(Self::args(key)),
            VIEW_TIMEOUT,
        )
        .map_err(|e| Failure::Unavailable(format!("running `{}`: {e}", self.command)))?;

        match output.status.success() {
            true => Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
            false => Err(Failure::Refused(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            )),
        }
    }
}

pub struct JiraProvider<S: Shell> {
    shell: S,
}

impl JiraProvider<AcliShell> {
    pub fn new() -> Self {
        Self {
            shell: AcliShell::default(),
        }
    }
}

impl Default for JiraProvider<AcliShell> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Shell> JiraProvider<S> {
    pub fn with_shell(shell: S) -> Self {
        Self { shell }
    }
}

impl<S: Shell> IssueProvider for JiraProvider<S> {
    /// One call per key: `acli`'s `view` takes a single key on the command
    /// line, and batching would mean writing a temp file for it. At a handful
    /// of watched issues that trade isn't close.
    fn read(&self, keys: &[String]) -> Result<Vec<IssueLookup>, ProviderError> {
        keys.iter()
            .map(|key| {
                let status = match self.shell.view(key) {
                    Ok(raw) => interpret(&raw),
                    Err(Failure::Refused(message)) => Err(refusal(&message)),
                    // Nothing else will work either, so this is the whole
                    // provider failing rather than one issue.
                    Err(Failure::Unavailable(message)) => {
                        return Err(ProviderError::Transport(message));
                    }
                };
                Ok((key.clone(), status))
            })
            .collect()
    }
}

/// Recorded: `✗ Error: Issue does not exist or you do not have permission to
/// see it.` — which is exactly what `NotFound` already means.
fn refusal(message: &str) -> RefError {
    let cleaned = message.trim_start_matches(['✗', ' ']).trim();
    match cleaned.contains("does not exist") || cleaned.contains("do not have permission") {
        true => RefError::NotFound,
        false => RefError::Other(cleaned.to_string()),
    }
}

fn interpret(raw: &str) -> Result<IssueStatus, RefError> {
    // A single key can come back as the resource or as a one-element list —
    // `view` also reads keys from a file — so accept either. Branch on the
    // first character rather than trying one shape and falling back to the
    // other: serde reads a struct from a JSON array positionally, so a list
    // parses "successfully" into an issue with every field defaulted.
    let describe = |e: serde_json::Error| {
        // Name what arrived: the failure is almost always a different `acli`
        // version, and the output is the only clue to that.
        RefError::Other(format!(
            "could not read acli output ({e}): {}",
            snippet(raw)
        ))
    };
    let issue = match raw.trim_start().starts_with('[') {
        true => serde_json::from_str::<Vec<Issue>>(raw)
            .map_err(describe)?
            .into_iter()
            .next()
            .ok_or(RefError::NotFound)?,
        false => serde_json::from_str::<Issue>(raw).map_err(describe)?,
    };

    let Some(status) = issue.fields.status else {
        return Err(RefError::Other("acli reported no status field".into()));
    };
    Ok(IssueStatus {
        name: status.name,
        category: category(status.status_category.map(|c| c.key).unwrap_or_default()),
        summary: issue.fields.summary,
    })
}

/// Jira promises these three keys and nothing else. An unfamiliar one reads
/// as in-progress, so a watch keeps running rather than declaring a finish
/// Jira didn't report.
fn category(key: String) -> Category {
    match key.as_str() {
        "new" => Category::New,
        "done" => Category::Done,
        _ => Category::Indeterminate,
    }
}

fn snippet(raw: &str) -> String {
    let trimmed = raw.trim();
    match trimmed.char_indices().nth(120) {
        Some((cut, _)) => format!("{}…", &trimmed[..cut]),
        None => trimmed.to_string(),
    }
}

#[derive(Debug, Deserialize)]
struct Issue {
    #[serde(default)]
    fields: Fields,
}

#[derive(Debug, Deserialize, Default)]
struct Fields {
    #[serde(default)]
    summary: String,
    #[serde(default)]
    status: Option<Status>,
}

#[derive(Debug, Deserialize)]
struct Status {
    #[serde(default)]
    name: String,
    #[serde(rename = "statusCategory", default)]
    status_category: Option<StatusCategory>,
}

#[derive(Debug, Deserialize)]
struct StatusCategory {
    #[serde(default)]
    key: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// The REST issue resource, trimmed to the fields `--fields` asks for.
    ///
    /// Built from Jira's documented shape rather than recorded: the sandbox
    /// project this was developed against refuses both reads and writes with
    /// the installed `acli` (see docs/watching.md). The refusal fixture below
    /// *is* recorded, which is the half most likely to be wrong.
    fn issue(status: &str, category: &str) -> String {
        format!(
            r#"{{"id":"10001","key":"AI-1","fields":{{"summary":"Try the watch feature",
            "status":{{"name":"{status}","id":"3",
            "statusCategory":{{"id":4,"key":"{category}","name":"In Progress"}}}}}}}}"#
        )
    }

    struct Canned {
        replies: RefCell<Vec<Result<String, Failure>>>,
        asked: RefCell<Vec<String>>,
    }

    impl Canned {
        fn of(replies: Vec<Result<String, Failure>>) -> Self {
            Self {
                replies: RefCell::new(replies),
                asked: RefCell::new(Vec::new()),
            }
        }
    }

    impl Shell for Canned {
        fn view(&self, key: &str) -> Result<String, Failure> {
            self.asked.borrow_mut().push(key.to_string());
            self.replies.borrow_mut().remove(0)
        }
    }

    fn read_one(reply: Result<String, Failure>) -> Result<IssueStatus, RefError> {
        let provider = JiraProvider::with_shell(Canned::of(vec![reply]));
        provider
            .read(&["AI-1".to_string()])
            .expect("the provider itself ran")
            .remove(0)
            .1
    }

    #[test]
    fn a_status_becomes_a_name_and_a_category() {
        let status = read_one(Ok(issue("In Progress", "indeterminate"))).unwrap();
        assert_eq!(status.name, "In Progress");
        assert_eq!(status.category, Category::Indeterminate);
        assert_eq!(status.summary, "Try the watch feature");
    }

    #[test]
    fn every_documented_category_maps() {
        for (key, expected) in [
            ("new", Category::New),
            ("indeterminate", Category::Indeterminate),
            ("done", Category::Done),
        ] {
            let status = read_one(Ok(issue("Whatever", key))).unwrap();
            assert_eq!(status.category, expected, "{key}");
        }
    }

    #[test]
    fn an_unfamiliar_category_keeps_the_watch_running() {
        // Reading it as done would end the watch on a word Jira never
        // promised. Better to keep asking.
        let status = read_one(Ok(issue("Blocked", "undefined"))).unwrap();
        assert_eq!(status.category, Category::Indeterminate);
    }

    #[test]
    fn a_refusal_recorded_from_acli_reads_as_not_found() {
        // Recorded 2026-09-20 against acli 1.3.19: exit 1, empty stdout, this
        // line on stderr.
        let refused = Err(Failure::Refused(
            "✗ Error: Issue does not exist or you do not have permission to see it.".into(),
        ));
        assert_eq!(read_one(refused), Err(RefError::NotFound));
    }

    #[test]
    fn another_refusal_keeps_its_message() {
        let refused = Err(Failure::Refused("✗ Error: rate limit exceeded".into()));
        assert_eq!(
            read_one(refused),
            Err(RefError::Other("Error: rate limit exceeded".into()))
        );
    }

    #[test]
    fn a_missing_binary_fails_the_provider_not_one_issue() {
        // Retrying the other keys would just spawn more processes that also
        // don't exist.
        let provider = JiraProvider::with_shell(Canned::of(vec![Err(Failure::Unavailable(
            "running `acli`: No such file or directory".into(),
        ))]));
        assert!(provider.read(&["AI-1".to_string()]).is_err());
    }

    #[test]
    fn output_that_is_not_an_issue_says_what_arrived() {
        // The likeliest real failure is a different acli version, and the
        // output it did produce is the only clue to that.
        let error = read_one(Ok("acli: unknown flag --fields\n".into())).unwrap_err();
        let RefError::Other(message) = error else {
            panic!("expected a described failure");
        };
        assert!(message.contains("unknown flag"), "{message}");
    }

    #[test]
    fn a_single_issue_is_read_whether_or_not_it_is_wrapped_in_a_list() {
        let bare = issue("To Do", "new");
        let listed = format!("[{bare}]");
        assert_eq!(
            read_one(Ok(bare)).unwrap().name,
            read_one(Ok(listed)).unwrap().name
        );
    }

    #[test]
    fn each_key_is_asked_for_separately() {
        let provider = JiraProvider::with_shell(Canned::of(vec![
            Ok(issue("To Do", "new")),
            Ok(issue("Done", "done")),
        ]));
        let read = provider
            .read(&["AI-1".to_string(), "AI-2".to_string()])
            .unwrap();

        assert_eq!(read.len(), 2);
        assert_eq!(read[0].0, "AI-1");
        assert_eq!(read[1].1.as_ref().unwrap().category, Category::Done);
    }

    #[test]
    fn the_invocation_asks_only_for_the_fields_that_are_read() {
        let args = AcliShell::args("AI-1");
        assert_eq!(args[3], "AI-1");
        assert!(args.contains(&"--json"), "{args:?}");
        // Dropping --fields pulls a whole ADF description per poll.
        let fields = args.iter().position(|a| *a == "--fields").unwrap();
        assert_eq!(args[fields + 1], "key,summary,status");
    }
}
