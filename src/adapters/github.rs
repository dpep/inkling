//! GitHub pull-request state, over the GraphQL API.
//!
//! One request per batch, using field aliases so N pull requests cost one
//! round trip. GitHub reports a per-alias `null` plus an `errors` entry when
//! one repo is unreadable, and still resolves the rest — which is exactly the
//! in-band failure the port asks for.

use crate::domain::model::{Reference, State};
use crate::ports::{Lookup, ProviderError, RefError, RefStatus, StatusProvider};
use crate::secret::Secret;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

pub struct GitHubProvider {
    api_url: String,
    token: Secret,
    timeout: Duration,
}

impl GitHubProvider {
    pub fn new(api_url: impl Into<String>, token: Secret) -> Self {
        Self {
            api_url: api_url.into(),
            token,
            timeout: Duration::from_secs(30),
        }
    }

    fn post(&self, query: &str) -> Result<Response, ProviderError> {
        let body = serde_json::json!({ "query": query });
        let response = ureq::post(&self.api_url)
            .config()
            .timeout_global(Some(self.timeout))
            .build()
            .header("Authorization", &format!("Bearer {}", self.token.expose()))
            .header("User-Agent", "inkling")
            .send_json(&body);

        match response {
            Ok(mut ok) => ok
                .body_mut()
                .read_json::<Response>()
                .map_err(|e| ProviderError::Transport(format!("malformed response: {e}"))),
            // The message is GitHub's, but the token must never ride along in
            // an error string, so only the status is reported.
            Err(ureq::Error::StatusCode(code)) => {
                Err(ProviderError::Transport(format!("HTTP {code}")))
            }
            Err(e) => Err(ProviderError::Transport(e.to_string())),
        }
    }
}

impl StatusProvider for GitHubProvider {
    fn lookup(&self, references: &[Reference]) -> Result<Vec<Lookup>, ProviderError> {
        if references.is_empty() {
            return Ok(Vec::new());
        }
        let response = self.post(&build_query(references))?;
        Ok(interpret(references, &response))
    }
}

/// Aliases are positional (`r0`, `r1`, …) so results map back to the input
/// slice without depending on GitHub preserving order.
fn build_query(references: &[Reference]) -> String {
    let mut query = String::from("query {");
    for (i, reference) in references.iter().enumerate() {
        let Reference::GitHubPullRequest {
            owner,
            repo,
            number,
            ..
        } = reference;
        query.push_str(&format!(
            r#" r{i}: repository(owner: {}, name: {}) {{ pullRequest(number: {number}) {{ isDraft state reviewDecision author {{ login }} }} }}"#,
            quote(owner),
            quote(repo),
        ));
    }
    query.push_str(" }");
    query
}

/// GraphQL string literal. Owner and repo names come from a parsed URL, but
/// escaping them keeps a crafted link from reshaping the query.
fn quote(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn interpret(references: &[Reference], response: &Response) -> Vec<Lookup> {
    // Errors are keyed by the alias in their `path`, so a per-reference
    // reason survives instead of collapsing into "something failed".
    let mut errors: HashMap<&str, &str> = HashMap::new();
    for error in &response.errors {
        if let Some(alias) = error.path.first() {
            errors.insert(alias.as_str(), error.message.as_str());
        }
    }

    references
        .iter()
        .enumerate()
        .map(|(i, reference)| {
            let alias = format!("r{i}");
            let state = match response.data.get(&alias) {
                Some(Some(repository)) => match &repository.pull_request {
                    Some(pr) => Ok(RefStatus {
                        state: pr.state(),
                        author: pr
                            .author
                            .as_ref()
                            .map(|a| a.login.clone())
                            .unwrap_or_default(),
                    }),
                    None => Err(RefError::NotFound),
                },
                // Null data with an error is the normal unreadable-repo shape.
                _ => Err(match errors.get(alias.as_str()) {
                    Some(message) if message.contains("Could not resolve") => RefError::NotFound,
                    Some(message) => RefError::Other((*message).to_string()),
                    None => RefError::NotFound,
                }),
            };
            (reference.clone(), state)
        })
        .collect()
}

#[derive(Debug, Deserialize, Default)]
struct Response {
    #[serde(default)]
    data: HashMap<String, Option<Repository>>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Debug, Deserialize)]
struct Repository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<PullRequest>,
}

#[derive(Debug, Deserialize)]
struct PullRequest {
    #[serde(rename = "isDraft")]
    is_draft: bool,
    state: String,
    #[serde(rename = "reviewDecision")]
    review_decision: Option<String>,
    #[serde(default)]
    author: Option<Author>,
}

#[derive(Debug, Deserialize)]
struct Author {
    #[serde(default)]
    login: String,
}

impl PullRequest {
    /// GitHub reports merge status, draft status, and review decision
    /// independently. Inkling owns the collapse into one lifecycle position —
    /// this is the business logic, and it deliberately lives here rather than
    /// being asked of a model.
    fn state(&self) -> State {
        match self.state.as_str() {
            "MERGED" => State::Merged,
            "CLOSED" => State::Closed,
            _ if self.is_draft => State::Draft,
            // CHANGES_REQUESTED and REVIEW_REQUIRED both read as Open: the PR
            // is live and unapproved, and Inkling draws nothing for either.
            _ if self.review_decision.as_deref() == Some("APPROVED") => State::Approved,
            _ => State::Open,
        }
    }
}

#[derive(Debug, Deserialize)]
struct GraphQlError {
    message: String,
    #[serde(default)]
    path: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(repo: &str, number: u64) -> Reference {
        Reference::GitHubPullRequest {
            host: "github.com".into(),
            owner: "dpep".into(),
            repo: repo.into(),
            number,
        }
    }

    fn parse(json: &str) -> Response {
        serde_json::from_str(json).expect("valid response")
    }

    // Recorded from the real API while building this adapter.
    const MERGED: &str = r#"{"data":{"r0":{"pullRequest":{"isDraft":false,"state":"MERGED","reviewDecision":null}}}}"#;
    const OPEN: &str =
        r#"{"data":{"r0":{"pullRequest":{"isDraft":false,"state":"OPEN","reviewDecision":null}}}}"#;

    #[test]
    fn a_merged_pull_request_reads_as_merged() {
        let states = interpret(&[pr("gqls", 1)], &parse(MERGED));
        assert_eq!(states[0].1.as_ref().map(|s| s.state), Ok(State::Merged));
    }

    #[test]
    fn an_open_unreviewed_pull_request_reads_as_open() {
        let states = interpret(&[pr("polyid", 43)], &parse(OPEN));
        assert_eq!(states[0].1.as_ref().map(|s| s.state), Ok(State::Open));
    }

    #[test]
    fn review_decision_only_matters_while_the_pull_request_is_live() {
        let approved = r#"{"data":{"r0":{"pullRequest":{"isDraft":false,"state":"OPEN","reviewDecision":"APPROVED"}}}}"#;
        assert_eq!(
            interpret(&[pr("x", 1)], &parse(approved))[0]
                .1
                .as_ref()
                .map(|s| s.state),
            Ok(State::Approved)
        );

        // Approved then closed: closed wins, because it's what's true now.
        let closed = r#"{"data":{"r0":{"pullRequest":{"isDraft":false,"state":"CLOSED","reviewDecision":"APPROVED"}}}}"#;
        assert_eq!(
            interpret(&[pr("x", 1)], &parse(closed))[0]
                .1
                .as_ref()
                .map(|s| s.state),
            Ok(State::Closed)
        );
    }

    #[test]
    fn changes_requested_reads_as_open() {
        // Live and unapproved. Inkling draws nothing either way, and a
        // separate state would need retraction to be honest.
        let json = r#"{"data":{"r0":{"pullRequest":{"isDraft":false,"state":"OPEN","reviewDecision":"CHANGES_REQUESTED"}}}}"#;
        assert_eq!(
            interpret(&[pr("x", 1)], &parse(json))[0]
                .1
                .as_ref()
                .map(|s| s.state),
            Ok(State::Open)
        );
    }

    #[test]
    fn a_draft_reads_as_draft_even_once_approved() {
        let json = r#"{"data":{"r0":{"pullRequest":{"isDraft":true,"state":"OPEN","reviewDecision":"APPROVED"}}}}"#;
        assert_eq!(
            interpret(&[pr("x", 1)], &parse(json))[0]
                .1
                .as_ref()
                .map(|s| s.state),
            Ok(State::Draft)
        );
    }

    #[test]
    fn one_unreadable_repo_does_not_take_down_the_batch() {
        // The real shape, recorded: null data for the failed alias, an error
        // carrying its path, and the siblings resolved as normal.
        let json = r#"{
            "data": {
                "r0": {"pullRequest": {"isDraft": false, "state": "MERGED", "reviewDecision": null}},
                "r1": null,
                "r2": {"pullRequest": {"isDraft": false, "state": "OPEN", "reviewDecision": null}}
            },
            "errors": [{
                "type": "NOT_FOUND",
                "path": ["r1"],
                "message": "Could not resolve to a Repository with the name 'dpep/nope-not-real'."
            }]
        }"#;
        let refs = [pr("gqls", 1), pr("nope-not-real", 1), pr("polyid", 43)];
        let states = interpret(&refs, &parse(json));

        assert_eq!(states[0].1.as_ref().map(|s| s.state), Ok(State::Merged));
        assert_eq!(states[1].1, Err(RefError::NotFound));
        assert_eq!(states[2].1.as_ref().map(|s| s.state), Ok(State::Open));
    }

    #[test]
    fn the_pull_request_author_comes_back_with_the_state() {
        let json = r#"{"data":{"r0":{"pullRequest":{"isDraft":false,"state":"OPEN",
            "reviewDecision":null,"author":{"login":"octocat"}}}}}"#;
        let status = interpret(&[pr("x", 1)], &parse(json))[0].1.clone().unwrap();
        assert_eq!(status.author, "octocat");

        // A deleted account reports no author; that isn't a failure.
        let anon = r#"{"data":{"r0":{"pullRequest":{"isDraft":false,"state":"OPEN",
            "reviewDecision":null,"author":null}}}}"#;
        let status = interpret(&[pr("x", 1)], &parse(anon))[0].1.clone().unwrap();
        assert!(status.author.is_empty());
    }

    #[test]
    fn a_missing_pull_request_in_a_readable_repo_is_not_found() {
        let json = r#"{"data":{"r0":{"pullRequest":null}}}"#;
        assert_eq!(
            interpret(&[pr("gqls", 999)], &parse(json))[0].1,
            Err(RefError::NotFound)
        );
    }

    #[test]
    fn an_unexpected_error_keeps_its_message() {
        let json = r#"{
            "data": {"r0": null},
            "errors": [{"path": ["r0"], "message": "API rate limit exceeded"}]
        }"#;
        assert_eq!(
            interpret(&[pr("x", 1)], &parse(json))[0].1,
            Err(RefError::Other("API rate limit exceeded".into()))
        );
    }

    #[test]
    fn results_map_back_by_alias_not_by_response_order() {
        let json = r#"{"data":{
            "r1": {"pullRequest": {"isDraft": false, "state": "OPEN", "reviewDecision": null}},
            "r0": {"pullRequest": {"isDraft": false, "state": "MERGED", "reviewDecision": null}}
        }}"#;
        let refs = [pr("gqls", 1), pr("polyid", 43)];
        let states = interpret(&refs, &parse(json));
        assert_eq!(states[0].0, refs[0]);
        assert_eq!(states[0].1.as_ref().map(|s| s.state), Ok(State::Merged));
        assert_eq!(states[1].1.as_ref().map(|s| s.state), Ok(State::Open));
    }

    #[test]
    fn the_query_aliases_every_reference() {
        let query = build_query(&[pr("gqls", 1), pr("polyid", 43)]);
        assert!(
            query.contains(r#"r0: repository(owner: "dpep", name: "gqls")"#),
            "{query}"
        );
        assert!(query.contains("pullRequest(number: 43)"), "{query}");
    }

    #[test]
    fn repository_names_are_escaped_into_the_query() {
        let reference = Reference::GitHubPullRequest {
            host: "github.com".into(),
            owner: "dpep".into(),
            repo: r#"eviL") { x } #"#.into(),
            number: 1,
        };
        let query = build_query(&[reference]);
        // The quote that would have closed the string literal is escaped, so
        // the injected braces stay inert data.
        assert!(query.contains(r#"eviL\""#), "{query}");
    }
}
