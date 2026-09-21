//! GitHub pull-request state, over the GraphQL API.
//!
//! One request per batch, using field aliases so N pull requests cost one
//! round trip. GitHub reports a per-alias `null` plus an `errors` entry when
//! one repo is unreadable, and still resolves the rest — which is exactly the
//! in-band failure the port asks for.

use crate::domain::detect::Pushed;
use crate::domain::model::{Reference, State};
use crate::domain::wait::CheckState;
use crate::domain::watch::{FailedCheck, Head};
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

    fn post<T: serde::de::DeserializeOwned>(&self, query: &str) -> Result<T, ProviderError> {
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
                .read_json::<T>()
                .map_err(|e| ProviderError::Transport(format!("malformed response: {e}"))),
            // The message is GitHub's, but the token must never ride along in
            // an error string, so only the status is reported.
            Err(ureq::Error::StatusCode(code)) => {
                Err(ProviderError::Transport(format!("HTTP {code}")))
            }
            Err(e) => Err(ProviderError::Transport(e.to_string())),
        }
    }

    /// The open pull requests each push updated, as `(index into pushes,
    /// pull request, its head)`. One request for all of them.
    ///
    /// A pull request counts only if its head is exactly the repository and
    /// branch that were pushed. Asked of the pushed repository and of its
    /// parent, because a fork's pull requests live upstream — where anybody
    /// else's fork may have a branch of the same name.
    pub fn pushed_to(
        &self,
        pushes: &[Pushed],
    ) -> Result<Vec<(usize, Reference, Head)>, ProviderError> {
        if pushes.is_empty() {
            return Ok(Vec::new());
        }
        let response: PushResponse = self.post(&push_query(pushes))?;
        Ok(pushed_pulls(pushes, &response))
    }
}

fn push_query(pushes: &[Pushed]) -> String {
    let mut query = String::from("query {");
    for (i, push) in pushes.iter().enumerate() {
        // `headRefName` can't be narrowed to one fork, and upstream `master`
        // is every stranger's branch too — recorded: five open ones on
        // rust-lang/rust. A page of 100 keeps yours from falling off the end;
        // past that a push watches nothing, never the wrong one.
        let pulls = format!(
            "nameWithOwner pullRequests(headRefName: {}, states: OPEN, first: 100) {{ nodes {{ number headRefName headRepository {{ nameWithOwner }} }} }}",
            quote(&push.branch)
        );
        query.push_str(&format!(
            " p{i}: repository(owner: {}, name: {}) {{ {pulls} parent {{ {pulls} }} }}",
            quote(&push.owner),
            quote(&push.repo),
        ));
    }
    query.push_str(" }");
    query
}

fn pushed_pulls(pushes: &[Pushed], response: &PushResponse) -> Vec<(usize, Reference, Head)> {
    let mut found = Vec::new();
    for (i, push) in pushes.iter().enumerate() {
        let Some(Some(pushed)) = response.data.get(&format!("p{i}")) else {
            continue;
        };
        for base in std::iter::once(pushed).chain(pushed.parent.as_deref()) {
            let Some((owner, repo)) = base.name_with_owner.split_once('/') else {
                continue;
            };
            for pull in &base.pull_requests.nodes {
                let Some(head) = pull.head() else { continue };
                if !head.is(&push.repository(), &push.branch) {
                    continue;
                }
                let reference = Reference::GitHubPullRequest {
                    host: push.host.clone(),
                    owner: owner.to_string(),
                    repo: repo.to_string(),
                    number: pull.number,
                };
                found.push((i, reference, head));
            }
        }
    }
    found
}

#[derive(Debug, Deserialize, Default)]
struct PushResponse {
    #[serde(default)]
    data: HashMap<String, Option<PushedRepository>>,
}

#[derive(Debug, Deserialize)]
struct PushedRepository {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
    #[serde(default, rename = "pullRequests")]
    pull_requests: HeadedPulls,
    #[serde(default)]
    parent: Option<Box<PushedRepository>>,
}

#[derive(Debug, Deserialize, Default)]
struct HeadedPulls {
    #[serde(default)]
    nodes: Vec<HeadedPull>,
}

#[derive(Debug, Deserialize)]
struct HeadedPull {
    number: u64,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    #[serde(default, rename = "headRepository")]
    head_repository: Option<NamedRepository>,
}

impl HeadedPull {
    fn head(&self) -> Option<Head> {
        Some(Head {
            repo: self.head_repository.as_ref()?.name_with_owner.clone(),
            branch: self.head_ref_name.clone(),
        })
    }
}

impl StatusProvider for GitHubProvider {
    fn lookup(&self, references: &[Reference]) -> Result<Vec<Lookup>, ProviderError> {
        // Answers only about pull requests. Dropping the rest rather than
        // reporting them missing is the honest shape: this provider has no
        // opinion on a Jira key, and the watch loop routes those elsewhere.
        let pulls: Vec<Reference> = references
            .iter()
            .filter(|r| matches!(r, Reference::GitHubPullRequest { .. }))
            .cloned()
            .collect();
        if pulls.is_empty() {
            return Ok(Vec::new());
        }
        let response = self.post(&build_query(&pulls))?;
        Ok(interpret(&pulls, &response))
    }
}

/// Aliases are positional (`r0`, `r1`, …) so results map back to the input
/// slice without depending on GitHub preserving order.
fn build_query(references: &[Reference]) -> String {
    let mut query = String::from("query {");
    for (i, reference) in references.iter().enumerate() {
        // The alias carries the index, so skipping one can't shift the rest.
        let Reference::GitHubPullRequest {
            owner,
            repo,
            number,
            ..
        } = reference
        else {
            continue;
        };
        query.push_str(&format!(
            r#" r{i}: repository(owner: {}, name: {}) {{ pullRequest(number: {number}) {{ isDraft state reviewDecision title headRefName headRepository {{ nameWithOwner }} author {{ login ... on User {{ name }} }} commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ state contexts(first: 20) {{ nodes {{ __typename ... on CheckRun {{ name conclusion databaseId checkSuite {{ app {{ slug }} }} }} }} }} }} }} }} }} }} }}"#,
            quote(owner),
            quote(repo),
        ));
    }
    for (j, (owner, repo)) in repositories(references).into_iter().enumerate() {
        query.push_str(&format!(
            r#" e{j}: repository(owner: {}, name: {}) {{ pullRequests(last: 5) {{ nodes {{ commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ state }} }} }} }} }} }} }}"#,
            quote(owner),
            quote(repo),
        ));
    }
    query.push_str(" }");
    query
}

/// Each repository once, in first-seen order — the order of the `e{j}` aliases.
fn repositories(references: &[Reference]) -> Vec<(&str, &str)> {
    let mut seen = Vec::new();
    for reference in references {
        if let Reference::GitHubPullRequest { owner, repo, .. } = reference {
            let key = (owner.as_str(), repo.as_str());
            if !seen.contains(&key) {
                seen.push(key);
            }
        }
    }
    seen
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

    // A head with no rollup is what a repository without CI reports, and also
    // what one with CI reports for the seconds after a push. Its recent pull
    // requests tell the two apart.
    let repositories = repositories(references);
    let runs_checks = |reference: &Reference| {
        let Reference::GitHubPullRequest { owner, repo, .. } = reference else {
            return false;
        };
        repositories
            .iter()
            .position(|&key| key == (owner.as_str(), repo.as_str()))
            .and_then(|j| response.data.get(&format!("e{j}")))
            .and_then(Option::as_ref)
            .is_some_and(Repository::runs_checks)
    };

    references
        .iter()
        .enumerate()
        .map(|(i, reference)| {
            let alias = format!("r{i}");
            let state = match response.data.get(&alias) {
                Some(Some(repository)) => match &repository.pull_request {
                    Some(pr) => Ok(RefStatus {
                        state: pr.state(),
                        author: pr.author.as_ref().map(Author::display).unwrap_or_default(),
                        title: pr.title.clone(),
                        checks: pr
                            .checks()
                            .or_else(|| runs_checks(reference).then_some(CheckState::Pending)),
                        failing: pr.failing(),
                        head: pr.head(),
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
    #[serde(default, rename = "pullRequest")]
    pull_request: Option<PullRequest>,
    /// Only on the `e{j}` aliases.
    #[serde(default, rename = "pullRequests")]
    recent: Option<RecentPullRequests>,
}

impl Repository {
    /// Whether any recent pull request's head had checks.
    fn runs_checks(&self) -> bool {
        let Some(recent) = &self.recent else {
            return false;
        };
        recent
            .nodes
            .iter()
            .flat_map(|pr| &pr.commits.nodes)
            .any(|node| node.commit.status_check_rollup.is_some())
    }
}

#[derive(Debug, Deserialize)]
struct RecentPullRequests {
    #[serde(default)]
    nodes: Vec<RecentPullRequest>,
}

#[derive(Debug, Deserialize)]
struct RecentPullRequest {
    #[serde(default)]
    commits: Commits,
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
    #[serde(default)]
    title: String,
    #[serde(default, rename = "headRefName")]
    head_ref_name: Option<String>,
    /// Null once a fork is deleted, and then nothing can be pushed to it.
    #[serde(default, rename = "headRepository")]
    head_repository: Option<NamedRepository>,
    #[serde(default)]
    commits: Commits,
}

#[derive(Debug, Deserialize)]
struct NamedRepository {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
}

/// Checks hang off the head commit, so the rollup arrives down this shaft
/// rather than on the pull request itself.
#[derive(Debug, Deserialize, Default)]
struct Commits {
    #[serde(default)]
    nodes: Vec<CommitNode>,
}

#[derive(Debug, Deserialize)]
struct CommitNode {
    commit: Commit,
}

#[derive(Debug, Deserialize)]
struct Commit {
    #[serde(rename = "statusCheckRollup")]
    status_check_rollup: Option<Rollup>,
}

#[derive(Debug, Deserialize)]
struct Rollup {
    state: String,
    #[serde(default)]
    contexts: Contexts,
}

/// The rollup's constituents. Measured 2026-09-20: a 3-reference batch costs
/// one rate-limit point with this field or without it, so naming the failing
/// check is free — same request, same point.
#[derive(Debug, Deserialize, Default)]
struct Contexts {
    #[serde(default)]
    nodes: Vec<Context>,
}

/// Only the `CheckRun` half of the union is selected. A repository still on
/// legacy commit statuses reports a red rollup with no name attached, which
/// is the old behavior rather than a new failure.
#[derive(Debug, Deserialize)]
struct Context {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default, rename = "databaseId")]
    database_id: Option<u64>,
    #[serde(default, rename = "checkSuite")]
    check_suite: Option<CheckSuite>,
}

impl Context {
    /// An Actions check run's id is its job's id. Any other app's is not —
    /// rust-lang/rust's `Bors try build` is a check run with no job behind it.
    fn job(&self) -> Option<u64> {
        let slug = self.check_suite.as_ref()?.app.as_ref()?.slug.as_str();
        (slug == "github-actions").then_some(self.database_id?)
    }
}

#[derive(Debug, Deserialize)]
struct CheckSuite {
    #[serde(default)]
    app: Option<App>,
}

#[derive(Debug, Deserialize)]
struct App {
    slug: String,
}

#[derive(Debug, Deserialize)]
struct Author {
    #[serde(default)]
    login: String,
    /// Real name, when the account has one and is a User (bots don't).
    #[serde(default)]
    name: Option<String>,
}

impl Author {
    /// Prefer the human name; a login is a fallback, not an identity.
    fn display(&self) -> String {
        match self.name.as_deref() {
            Some(name) if !name.trim().is_empty() => name.to_string(),
            _ => self.login.clone(),
        }
    }
}

impl PullRequest {
    /// GitHub reports merge status, draft status, and review decision
    /// independently. Inkling owns the collapse into one lifecycle position —
    /// this is the business logic, and it deliberately lives here rather than
    /// being asked of a model.
    /// GitHub reports more rollup states than matter here. Anything not
    /// plainly finished reads as pending, so an unfamiliar one makes a wait
    /// keep waiting rather than declare a result it didn't get.
    fn rollup(&self) -> Option<&Rollup> {
        self.commits
            .nodes
            .first()?
            .commit
            .status_check_rollup
            .as_ref()
    }

    /// The first check that actually failed.
    ///
    /// Not "the first one that isn't green": recorded from rust-lang/rust the
    /// list runs successes, then a `SKIPPED`, then the failure — so anything
    /// looser names a job that did nothing wrong.
    fn failing(&self) -> Option<FailedCheck> {
        let context = self.rollup()?.contexts.nodes.iter().find(|context| {
            matches!(
                context.conclusion.as_deref(),
                Some("FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "STARTUP_FAILURE")
            )
        })?;
        let name = context
            .name
            .clone()
            .filter(|name| !name.trim().is_empty())?;
        Some(FailedCheck {
            name,
            job: context.job(),
        })
    }

    fn head(&self) -> Option<Head> {
        Some(Head {
            repo: self.head_repository.as_ref()?.name_with_owner.clone(),
            branch: self.head_ref_name.clone()?,
        })
    }

    fn checks(&self) -> Option<CheckState> {
        let state = self.rollup()?.state.as_str();
        Some(match state {
            "SUCCESS" => CheckState::Success,
            // ERROR is a check that failed to run; for someone waiting to
            // respond, that is the same news as a failing one.
            "FAILURE" | "ERROR" => CheckState::Failure,
            _ => CheckState::Pending,
        })
    }

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
            "reviewDecision":null,"author":{"login":"dpep","name":"Daniel Pepper"}}}}}"#;
        let status = interpret(&[pr("x", 1)], &parse(json))[0].1.clone().unwrap();
        assert_eq!(status.author, "Daniel Pepper", "a name beats a login");

        // Bots and name-less accounts still identify as something.
        let bare = r#"{"data":{"r0":{"pullRequest":{"isDraft":false,"state":"OPEN",
            "reviewDecision":null,"author":{"login":"dependabot"}}}}}"#;
        let status = interpret(&[pr("x", 1)], &parse(bare))[0].1.clone().unwrap();
        assert_eq!(status.author, "dependabot");

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

    /// The four rollup shapes, recorded from the live API (docs/waiting.md,
    /// W0). Trimmed to the checks path; the lifecycle fields are asserted
    /// elsewhere.
    fn with_rollup(rollup: &str) -> String {
        format!(
            r#"{{"data":{{"r0":{{"pullRequest":{{"isDraft":false,"state":"OPEN",
            "reviewDecision":null,"commits":{{"nodes":[{{"commit":{{"statusCheckRollup":{rollup}}}}}]}}}}}}}}}}"#
        )
    }

    #[test]
    fn every_recorded_rollup_shape_maps_to_a_check_state() {
        // SUCCESS from dpep/inkling#1, PENDING from rust-lang/rust#161731,
        // FAILURE from rust-lang/rust#161685.
        for (rollup, expected) in [
            (r#"{"state":"SUCCESS"}"#, Some(CheckState::Success)),
            (r#"{"state":"PENDING"}"#, Some(CheckState::Pending)),
            (r#"{"state":"FAILURE"}"#, Some(CheckState::Failure)),
            // sinatra/sinatra#100 — merged before Actions existed.
            ("null", None),
        ] {
            let status = interpret(&[pr("x", 1)], &parse(&with_rollup(rollup)))[0]
                .1
                .clone()
                .unwrap();
            assert_eq!(status.checks, expected, "rollup {rollup}");
        }
    }

    /// Recorded 2026-09-20 from dpep/inkling-ci-probe#2, one second after it
    /// was opened: the head has no rollup yet — it got one two seconds later —
    /// but the repository's recent pull requests show it runs checks.
    fn fresh_push(recent: &str) -> String {
        format!(
            r#"{{"data":{{"r0":{{"pullRequest":{{"isDraft":true,"state":"OPEN","reviewDecision":null,
            "commits":{{"nodes":[{{"commit":{{"statusCheckRollup":null}}}}]}}}}}},
            "e0":{{"pullRequests":{{"nodes":[{recent}]}}}}}}}}"#
        )
    }

    fn head(rollup: &str) -> String {
        format!(r#"{{"commits":{{"nodes":[{{"commit":{{"statusCheckRollup":{rollup}}}}}]}}}}"#)
    }

    #[test]
    fn a_fresh_push_to_a_repository_that_runs_checks_reads_as_pending() {
        // dpep/inkling#13: read as "no checks", this ended a wait at 0s.
        let recent = [head(r#"{"state":"FAILURE"}"#), head("null")].join(",");
        let status = interpret(&[pr("inkling-ci-probe", 2)], &parse(&fresh_push(&recent)))[0]
            .1
            .clone()
            .unwrap();
        assert_eq!(status.checks, Some(CheckState::Pending));
    }

    #[test]
    fn a_repository_whose_recent_pull_requests_ran_nothing_has_no_checks() {
        // The no-CI answer must stay immediate.
        let recent = [head("null"), head("null")].join(",");
        let status = interpret(&[pr("x", 1)], &parse(&fresh_push(&recent)))[0]
            .1
            .clone()
            .unwrap();
        assert_eq!(status.checks, None);
    }

    #[test]
    fn a_pull_request_with_no_commits_reports_no_checks() {
        // Not the same as no checks configured, but indistinguishable from
        // here and identical in consequence: there is nothing to wait for.
        let json = r#"{"data":{"r0":{"pullRequest":{"isDraft":false,"state":"OPEN",
            "reviewDecision":null,"commits":{"nodes":[]}}}}}"#;
        let status = interpret(&[pr("x", 1)], &parse(json))[0].1.clone().unwrap();
        assert_eq!(status.checks, None);
    }

    #[test]
    fn an_unfamiliar_rollup_state_reads_as_pending() {
        // Better to keep waiting than to report a result GitHub didn't give.
        // EXPECTED and QUEUED are real values; the point is the default.
        for rollup in [r#"{"state":"EXPECTED"}"#, r#"{"state":"WHAT_IS_THIS"}"#] {
            let status = interpret(&[pr("x", 1)], &parse(&with_rollup(rollup)))[0]
                .1
                .clone()
                .unwrap();
            assert_eq!(status.checks, Some(CheckState::Pending), "rollup {rollup}");
        }
    }

    #[test]
    fn a_check_that_errored_counts_as_failed() {
        let status = interpret(&[pr("x", 1)], &parse(&with_rollup(r#"{"state":"ERROR"}"#)))[0]
            .1
            .clone()
            .unwrap();
        assert_eq!(status.checks, Some(CheckState::Failure));
    }

    /// Recorded 2026-09-20 from rust-lang/rust#161685, trimmed from 15
    /// contexts to the shape that matters: successes, then a `SKIPPED`, then
    /// the actual failure.
    const RED_WITH_CONTEXTS: &str = r#"{"data":{"r0":{"pullRequest":{"isDraft":false,
        "state":"CLOSED","reviewDecision":"REVIEW_REQUIRED","title":"Rollup of 6 pull requests",
        "commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"FAILURE","contexts":{"nodes":[
        {"__typename":"CheckRun","name":"PR - tidy","conclusion":"SUCCESS"},
        {"__typename":"CheckRun","name":"publish toolstate","conclusion":"SKIPPED"},
        {"__typename":"CheckRun","name":"Bors auto build","conclusion":"FAILURE"},
        {"__typename":"CheckRun","name":"Bors try build","conclusion":"FAILURE"}
        ]}}}}]}}}}}"#;

    #[test]
    fn a_red_rollup_names_the_check_that_failed() {
        let status = interpret(&[pr("x", 1)], &parse(RED_WITH_CONTEXTS))[0]
            .1
            .clone()
            .unwrap();
        assert_eq!(status.checks, Some(CheckState::Failure));
        assert_eq!(
            status.failing.map(|f| f.name).as_deref(),
            Some("Bors auto build"),
            "the first job that failed, not the first that isn't green — a \
             SKIPPED job sits in between"
        );
    }

    /// Recorded 2026-09-20 from a scratch repository, mid-run: `lint` has
    /// failed while `test` is still going. `Bors try build` is from
    /// rust-lang/rust, a check run whose app is not Actions.
    fn red(first: &str) -> String {
        let actions = r#"{"__typename":"CheckRun","name":"lint","conclusion":"FAILURE",
            "databaseId":106192233819,"checkSuite":{"app":{"slug":"github-actions"}}}"#;
        let bors = r#"{"__typename":"CheckRun","name":"Bors try build","conclusion":"FAILURE",
            "databaseId":55,"checkSuite":{"app":null}}"#;
        let running = r#"{"__typename":"CheckRun","name":"test","conclusion":null,
            "databaseId":106192233589,"checkSuite":{"app":{"slug":"github-actions"}}}"#;
        let failed = if first == "bors" { bors } else { actions };
        with_rollup(&format!(
            r#"{{"state":"FAILURE","contexts":{{"nodes":[{failed},{running}]}}}}"#
        ))
    }

    #[test]
    fn a_red_actions_job_carries_its_job_id() {
        let status = interpret(&[pr("x", 1)], &parse(&red("actions")))[0]
            .1
            .clone()
            .unwrap();
        assert_eq!(
            status.failing,
            Some(FailedCheck {
                name: "lint".into(),
                job: Some(106192233819),
            })
        );
    }

    #[test]
    fn a_check_run_from_another_app_has_no_job_to_point_at() {
        let status = interpret(&[pr("x", 1)], &parse(&red("bors")))[0]
            .1
            .clone()
            .unwrap();
        assert_eq!(
            status.failing,
            Some(FailedCheck {
                name: "Bors try build".into(),
                job: None,
            })
        );
    }

    #[test]
    fn a_green_rollup_names_nothing() {
        let json = with_rollup(
            r#"{"state":"SUCCESS","contexts":{"nodes":[
            {"__typename":"CheckRun","name":"test","conclusion":"SUCCESS"}]}}"#,
        );
        assert_eq!(
            interpret(&[pr("x", 1)], &parse(&json))[0]
                .1
                .clone()
                .unwrap()
                .failing,
            None
        );
    }

    #[test]
    fn a_legacy_status_context_leaves_the_rollup_unnamed() {
        // Only the CheckRun half of the union is selected, so a repository on
        // old-style commit statuses reports red with nothing attached — which
        // is the behavior before this field existed, not a new failure.
        let json = with_rollup(
            r#"{"state":"FAILURE","contexts":{"nodes":[
            {"__typename":"StatusContext"}]}}"#,
        );
        let status = interpret(&[pr("x", 1)], &parse(&json))[0]
            .1
            .clone()
            .unwrap();
        assert_eq!(status.checks, Some(CheckState::Failure));
        assert_eq!(status.failing, None);
    }

    #[test]
    fn the_query_asks_for_the_head_commits_rollup_and_its_contexts() {
        let query = build_query(&[pr("gqls", 1)]);
        assert!(query.contains("commits(last: 1)"), "{query}");
        assert!(query.contains("statusCheckRollup"), "{query}");
        // Measured: this costs nothing extra — a 3-reference batch is one
        // rate-limit point with it or without it.
        assert!(query.contains("contexts(first: 20)"), "{query}");
        // So a red Actions job's log can be named; also free, measured the same way.
        assert!(
            query.contains("databaseId checkSuite { app { slug } }"),
            "{query}"
        );
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
    fn the_query_asks_each_repository_once_whether_it_runs_checks() {
        // Measured: 40 references with a per-reference ask cost 3 points;
        // asked once per repository, 1.
        let query = build_query(&[pr("gqls", 1), pr("gqls", 2), pr("polyid", 43)]);
        assert_eq!(query.matches("pullRequests(last: 5)").count(), 2, "{query}");
        assert!(
            query.contains(r#"e1: repository(owner: "dpep", name: "polyid")"#),
            "{query}"
        );
    }

    #[test]
    fn a_reading_names_the_branch_a_pull_request_comes_from() {
        assert!(
            build_query(&[pr("x", 1)]).contains("headRefName headRepository { nameWithOwner }")
        );
        let json = r#"{"data":{"r0":{"pullRequest":{"isDraft":false,"state":"OPEN","reviewDecision":null,
            "headRefName":"push-watch","headRepository":{"nameWithOwner":"dpep/inkling"}}}}}"#;
        let status = interpret(&[pr("x", 1)], &parse(json))[0].1.clone().unwrap();
        assert_eq!(
            status.head,
            Some(Head {
                repo: "dpep/inkling".into(),
                branch: "push-watch".into()
            })
        );
    }

    fn push(owner: &str, repo: &str, branch: &str) -> Pushed {
        Pushed {
            host: "github.com".into(),
            owner: owner.into(),
            repo: repo.into(),
            branch: branch.into(),
            invited: false,
        }
    }

    fn found(pushes: &[Pushed], json: &str) -> Vec<(usize, String, String)> {
        let response: PushResponse = serde_json::from_str(json).unwrap();
        pushed_pulls(pushes, &response)
            .into_iter()
            .map(|(i, reference, head)| (i, reference.to_string(), head.branch))
            .collect()
    }

    /// Recorded 2026-09-20. `p0` is a fork whose pull request lives upstream;
    /// `p1` is five strangers' forks whose branch is also called `master`.
    const PUSHED: &str = r#"{"data":{"p0":{"nameWithOwner":"notriddle/rust","pullRequests":{"nodes":[]},"parent":{"nameWithOwner":"rust-lang/rust","pullRequests":{"nodes":[{"number":163058,"headRefName":"rustdoc/separate-parsing","headRepository":{"nameWithOwner":"notriddle/rust"}}]}}},"p1":{"nameWithOwner":"rust-lang/rust","pullRequests":{"nodes":[{"number":131340,"headRefName":"master","headRepository":{"nameWithOwner":"eduardorittner/rust"}},{"number":140640,"headRefName":"master","headRepository":{"nameWithOwner":"kstrafe/rust"}}]},"parent":null}}}"#;

    #[test]
    fn a_push_to_a_fork_finds_its_pull_request_upstream() {
        let pushes = [
            push("notriddle", "rust", "rustdoc/separate-parsing"),
            push("rust-lang", "rust", "master"),
        ];
        assert_eq!(
            found(&pushes, PUSHED),
            [(
                0,
                "github.com/rust-lang/rust#163058".into(),
                "rustdoc/separate-parsing".into()
            )],
            "and not one of the strangers whose branch is also master"
        );
    }

    #[test]
    fn every_open_pull_request_from_the_pushed_branch_is_found() {
        // One branch, two bases: the push changed both.
        let json = r#"{"data":{"p0":{"nameWithOwner":"dpep/inkling","pullRequests":{"nodes":[
            {"number":7,"headRefName":"fix","headRepository":{"nameWithOwner":"dpep/inkling"}},
            {"number":9,"headRefName":"fix","headRepository":{"nameWithOwner":"dpep/inkling"}}]},"parent":null}}}"#;
        let refs: Vec<String> = found(&[push("dpep", "inkling", "fix")], json)
            .into_iter()
            .map(|(_, reference, _)| reference)
            .collect();
        assert_eq!(
            refs,
            ["github.com/dpep/inkling#7", "github.com/dpep/inkling#9"]
        );
    }

    #[test]
    fn an_unreadable_repository_or_a_deleted_fork_finds_nothing() {
        // Recorded: the null alias GitHub returns for a repository it can't resolve.
        let missing = r#"{"data":{"p0":null},"errors":[{"type":"NOT_FOUND","path":["p0"],"message":"Could not resolve to a Repository with the name 'dpep/does-not-exist-xyz'."}]}"#;
        assert!(found(&[push("dpep", "does-not-exist-xyz", "x")], missing).is_empty());
        let orphan = r#"{"data":{"p0":{"nameWithOwner":"dpep/inkling","pullRequests":{"nodes":[
            {"number":7,"headRefName":"fix","headRepository":null}]}}}}"#;
        assert!(found(&[push("dpep", "inkling", "fix")], orphan).is_empty());
    }

    #[test]
    fn the_push_query_asks_the_repository_and_its_parent_with_names_escaped() {
        let query = push_query(&[push("dpep", "inkling", r#"x") { y } #"#)]);
        assert!(
            query.contains(r#"p0: repository(owner: "dpep", name: "inkling")"#),
            "{query}"
        );
        assert!(query.contains(r#"headRefName: "x\")"#), "{query}");
        assert_eq!(query.matches("states: OPEN").count(), 2, "{query}");
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
