//! GitHub pull-request state, over the GraphQL API.
//!
//! One request per batch, using field aliases so N pull requests cost one
//! round trip. GitHub reports a per-alias `null` plus an `errors` entry when
//! one repo is unreadable, and still resolves the rest — which is exactly the
//! in-band failure the port asks for.

use crate::domain::detect::Pushed;
use crate::domain::model::{Reference, State};
use crate::domain::wait::CheckState;
use crate::domain::watch::{Actors, Discussion, FailedCheck, Head, Remark};
use crate::ports::{Lookup, ProviderError, RefError, RefStatus, StatusProvider};
use crate::secret::Secret;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

pub struct GitHubProvider {
    api_url: String,
    token: Secret,
    timeout: Duration,
    ignore_checks: Vec<String>,
}

impl GitHubProvider {
    pub fn new(api_url: impl Into<String>, token: Secret) -> Self {
        Self {
            api_url: api_url.into(),
            token,
            timeout: Duration::from_secs(30),
            ignore_checks: Vec::new(),
        }
    }

    pub fn from_config(github: &crate::config::GitHub, token: Secret) -> Self {
        Self {
            ignore_checks: github.ignore_checks.clone(),
            ..Self::new(&github.api_url, token)
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
        let mut response: Response = self.post(&build_query(&pulls))?;
        response.ignore(&self.ignore_checks);
        Ok(interpret(&pulls, &response))
    }
}

/// How many of the latest comments, and of the latest reviews, a reading
/// asks for. A burst bigger than this still reads as news, as "at least".
const PAGE: usize = 30;

/// Aliases are positional (`r0`, `r1`, …) so results map back to the input
/// slice without depending on GitHub preserving order.
fn build_query(references: &[Reference]) -> String {
    // Who the reader is rides along: one field, in a request being made
    // anyway, so there is nothing to cache and nothing to go stale.
    let mut query = String::from("query { viewer { login }");
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
            r#" r{i}: repository(owner: {}, name: {}) {{ pullRequest(number: {number}) {{ isDraft state reviewDecision mergedBy {{ __typename login }} title headRefName headRepository {{ nameWithOwner }} author {{ login ... on User {{ name }} }} latestReviews(first: 20) {{ nodes {{ state author {{ __typename login }} }} }} comments(last: {PAGE}) {{ totalCount nodes {{ id createdAt author {{ __typename login }} }} }} reviews(last: {PAGE}) {{ totalCount nodes {{ id state submittedAt body author {{ __typename login }} comments {{ totalCount }} }} }} timelineItems(last: 1, itemTypes: [READY_FOR_REVIEW_EVENT, CONVERT_TO_DRAFT_EVENT]) {{ nodes {{ ... on ReadyForReviewEvent {{ actor {{ __typename login }} }} ... on ConvertToDraftEvent {{ actor {{ __typename login }} }} }} }} commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ state contexts(first: 100) {{ totalCount nodes {{ __typename ... on CheckRun {{ name conclusion databaseId checkSuite {{ app {{ slug }} workflowRun {{ event workflow {{ name }} }} }} }} ... on StatusContext {{ context state }} }} }} }} }} }} }} }} }}"#,
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
    // An error with no path is about the query itself — a syntax error fails
    // the whole batch — and must not read as every reference not existing.
    let mut whole_batch = None;
    for error in &response.errors {
        match error.path.first() {
            Some(alias) => {
                errors.insert(alias.as_str(), error.message.as_str());
            }
            None => whole_batch = whole_batch.or(Some(error.message.as_str())),
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

    // Spelled as a pull request's actors are, so a comment can be told apart
    // as the reader's own.
    let me = response
        .data
        .viewer
        .as_ref()
        .map(|viewer| format!("@{}", viewer.login));

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
                        by: Actors {
                            me: me.clone(),
                            ..pr.actors()
                        },
                        discussion: Some(pr.discussion()),
                    }),
                    None => Err(RefError::NotFound),
                },
                // Null data with an error is the normal unreadable-repo shape.
                _ => Err(match errors.get(alias.as_str()).copied().or(whole_batch) {
                    Some(message) if message.contains("Could not resolve") => RefError::NotFound,
                    Some(message) => RefError::Other(message.to_string()),
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
    data: Data,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

/// The aliased repositories, and who is asking.
#[derive(Debug, Deserialize, Default)]
struct Data {
    #[serde(default)]
    viewer: Option<Viewer>,
    #[serde(flatten)]
    repositories: HashMap<String, Option<Repository>>,
}

impl Data {
    fn get(&self, alias: &str) -> Option<&Option<Repository>> {
        self.repositories.get(alias)
    }
}

#[derive(Debug, Deserialize)]
struct Viewer {
    login: String,
}

impl Response {
    /// Drop the checks configured as noise — a merge lock, a label policy —
    /// before anything judges the rollup, so they can neither turn it red nor
    /// be named as the check that did.
    fn ignore(&mut self, names: &[String]) {
        if names.is_empty() {
            return;
        }
        let rollups = self
            .data
            .repositories
            .values_mut()
            .flatten()
            .filter_map(|repository| repository.pull_request.as_mut())
            .flat_map(|pr| pr.commits.nodes.iter_mut())
            .filter_map(|node| node.commit.status_check_rollup.as_mut());
        for rollup in rollups {
            let contexts = &mut rollup.contexts;
            let before = contexts.nodes.len();
            contexts.nodes.retain(|context| {
                context
                    .display_name()
                    .is_none_or(|name| !names.iter().any(|ignored| ignored == name))
            });
            let dropped = before - contexts.nodes.len();
            contexts.total_count = contexts.total_count.saturating_sub(dropped);
            contexts.pruned |= dropped > 0;
        }
    }
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
    #[serde(default, rename = "mergedBy")]
    merged_by: Option<Reviewer>,
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
    #[serde(default, rename = "latestReviews")]
    latest_reviews: Option<Reviews>,
    /// The latest conversation comments.
    #[serde(default)]
    comments: Option<Page<IssueComment>>,
    /// The latest reviews, each carrying how many line comments it holds.
    #[serde(default)]
    reviews: Option<Page<SubmittedReview>>,
    /// The latest ready-for-review or convert-to-draft, whichever was last.
    #[serde(default, rename = "timelineItems")]
    draft_toggles: Option<Toggles>,
}

#[derive(Debug, Deserialize, Default)]
struct Toggles {
    #[serde(default)]
    nodes: Vec<Toggle>,
}

#[derive(Debug, Deserialize)]
struct Toggle {
    /// Null for a deleted account.
    #[serde(default)]
    actor: Option<Reviewer>,
}

/// Each reviewer's latest review, which is the one that stands.
#[derive(Debug, Deserialize, Default)]
struct Reviews {
    #[serde(default)]
    nodes: Vec<Review>,
}

#[derive(Debug, Deserialize)]
struct Review {
    state: String,
    /// Null for a deleted account.
    #[serde(default)]
    author: Option<Reviewer>,
}

#[derive(Debug, Deserialize)]
struct Reviewer {
    #[serde(rename = "__typename")]
    kind: String,
    login: String,
}

impl Reviewer {
    /// Logins only, never names: this reaches a session mid-turn, and a
    /// login's alphabet can't carry an instruction.
    fn display(&self) -> String {
        match self.kind.as_str() {
            "Bot" => format!("{} (bot)", self.login),
            _ => format!("@{}", self.login),
        }
    }
}

/// The last page of a connection, and how long the whole of it is.
#[derive(Debug, Deserialize)]
struct Page<T> {
    #[serde(default, rename = "totalCount")]
    total_count: u64,
    #[serde(default = "Vec::new")]
    nodes: Vec<T>,
}

impl<T> Page<T> {
    /// The oldest time on the page when the page didn't hold them all:
    /// anything older wasn't read.
    fn horizon(&self, at: impl Fn(&T) -> Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
        match self.total_count > self.nodes.len() as u64 {
            true => self.nodes.iter().filter_map(at).min(),
            false => None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct IssueComment {
    id: String,
    #[serde(rename = "createdAt")]
    created_at: DateTime<Utc>,
    /// Null for a deleted account.
    #[serde(default)]
    author: Option<Reviewer>,
}

/// A review as the conversation sees it. Its line comments are counted, not
/// read: they are published with the review, at its `submittedAt` — recorded
/// 2026-11-17, and a reply to a thread is a review of its own — so the review
/// says who and when for all of them.
#[derive(Debug, Deserialize)]
struct SubmittedReview {
    id: String,
    state: String,
    /// Null while the review is pending, which only its author can see.
    #[serde(default, rename = "submittedAt")]
    submitted_at: Option<DateTime<Utc>>,
    /// Whether it has one. The text itself is never kept.
    #[serde(default, deserialize_with = "has_text")]
    body: bool,
    #[serde(default)]
    author: Option<Reviewer>,
    #[serde(default)]
    comments: Option<Count>,
}

#[derive(Debug, Deserialize)]
struct Count {
    #[serde(default, rename = "totalCount")]
    total_count: u64,
}

/// Reads a string down to whether it says anything, so somebody else's prose
/// is never held past parsing.
fn has_text<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    let text: Option<String> = Option::deserialize(deserializer)?;
    Ok(text.is_some_and(|text| !text.trim().is_empty()))
}

/// What a deleted account is shown as.
const GHOST: &str = "@ghost";

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

/// The rollup's constituents. Measured 2026-09-20 and again 2026-09-22: a
/// 3-reference batch costs one rate-limit point with these fields or without
/// them, and one point still at `first: 100` returning 130 contexts — GitHub
/// charges for what comes back, so neither the page nor the fields cost
/// anything.
#[derive(Debug, Deserialize, Default)]
struct Contexts {
    /// How many there are, against however many fit on the page.
    #[serde(default, rename = "totalCount")]
    total_count: usize,
    #[serde(default)]
    nodes: Vec<Context>,
    /// Some were ignored, so GitHub's own state no longer sums what is left.
    #[serde(skip)]
    pruned: bool,
}

/// What tells one check from another: the same job name under a different
/// app, workflow, or triggering event is a different check.
type Identity<'a> = (&'a str, &'a str, &'a str, &'a str);

impl Contexts {
    /// The newest run of each check, in the order GitHub listed them.
    ///
    /// A context is one check *run*, not one check: a run superseded on the
    /// same commit stays here beside its replacement, keeping its conclusion
    /// forever. A context with no identity can't be shown to be superseded,
    /// so it stays.
    fn latest(&self) -> Vec<&Context> {
        let mut newest: HashMap<Identity<'_>, u64> = HashMap::new();
        for (identity, run) in self.nodes.iter().filter_map(Context::identity) {
            let seen = newest.entry(identity).or_insert(run);
            *seen = (*seen).max(run);
        }
        self.nodes
            .iter()
            .filter(|context| {
                context
                    .identity()
                    .is_none_or(|(identity, run)| newest[&identity] == run)
            })
            .collect()
    }

    /// What the checks now on the commit add up to, or `None` when they
    /// can't be read well enough to say.
    ///
    /// Three ways they can't, and one reason: what wasn't read can't be
    /// judged, and can't clear a check either. No contexts at all, more of
    /// them than the page held, or a member of the union this query doesn't
    /// select.
    fn verdict(&self) -> Option<CheckState> {
        let latest = self.latest();
        if latest.is_empty()
            || self.nodes.len() < self.total_count
            || latest.iter().any(|context| context.identity().is_none())
        {
            return None;
        }
        Some(if latest.iter().any(|context| context.failed()) {
            CheckState::Failure
        } else if latest.iter().any(|context| context.running()) {
            CheckState::Pending
        } else {
            // SKIPPED, NEUTRAL and STALE are left: none of them is a
            // verdict against the change, and GitHub's own rollup reads a
            // commit of successes and skips as SUCCESS.
            CheckState::Success
        })
    }
}

/// One member of the `CheckRun | StatusContext` union, flattened: a check run
/// fills `name` and `conclusion`, a commit status — Buildkite reports this
/// way — fills `context` and `state`.
#[derive(Debug, Deserialize)]
struct Context {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    state: Option<String>,
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

    /// Which check this is a run of, and which run of it.
    ///
    /// Keyed as `gh pr checks` keys it, whose view of a re-run commit this
    /// matches — with the app added, so two apps that happen to name a check
    /// the same aren't merged. Erring wide only ever keeps a spare entry;
    /// merging two real checks hides a failure.
    ///
    /// The run is the check run's id rather than `startedAt`, which `gh`
    /// sorts on: it is monotonic and always present, so it needs neither
    /// timestamp parsing nor a tie-break. A commit status has no run: GitHub
    /// keeps only the newest status per context, so each is its own check.
    /// `None` for a member of the union this query doesn't select.
    fn identity(&self) -> Option<(Identity<'_>, u64)> {
        if let Some(context) = self.context.as_deref() {
            return Some((("status", "", "", context), 0));
        }
        let name = self.name.as_deref()?;
        let run = self.database_id?;
        let suite = self.check_suite.as_ref();
        let app = suite
            .and_then(|suite| suite.app.as_ref())
            .map_or("", |app| app.slug.as_str());
        let workflow_run = suite.and_then(|suite| suite.workflow_run.as_ref());
        let event = workflow_run.map_or("", |run| run.event.as_str());
        let workflow = workflow_run
            .and_then(|run| run.workflow.as_ref())
            .map_or("", |workflow| workflow.name.as_str());
        Some(((app, workflow, event, name), run))
    }

    /// Conclusions that mean the check did not pass.
    ///
    /// `CANCELLED` among them: measured 2026-09-22 on a run cancelled with
    /// nothing replacing it, GitHub's rollup, `gh pr checks` and the merge
    /// box all call that red. Superseded cancellations, the noisy kind, are
    /// gone by the time this is asked — they are never the newest run.
    fn failed(&self) -> bool {
        matches!(
            self.conclusion.as_deref(),
            Some("FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "STARTUP_FAILURE")
        ) || matches!(self.state.as_deref(), Some("FAILURE" | "ERROR"))
    }

    /// Still to report: a check run without a conclusion, or a commit status
    /// that is pending or required but not yet posted (`EXPECTED`).
    fn running(&self) -> bool {
        match self.context {
            Some(_) => matches!(self.state.as_deref(), Some("PENDING" | "EXPECTED")),
            None => self.conclusion.is_none(),
        }
    }

    fn display_name(&self) -> Option<&str> {
        self.name
            .as_deref()
            .or(self.context.as_deref())
            .filter(|name| !name.trim().is_empty())
    }
}

#[derive(Debug, Deserialize)]
struct CheckSuite {
    #[serde(default)]
    app: Option<App>,
    #[serde(default, rename = "workflowRun")]
    workflow_run: Option<WorkflowRun>,
}

/// Null for a check run from an app that isn't Actions.
#[derive(Debug, Deserialize)]
struct WorkflowRun {
    #[serde(default)]
    event: String,
    #[serde(default)]
    workflow: Option<Workflow>,
}

#[derive(Debug, Deserialize)]
struct Workflow {
    #[serde(default)]
    name: String,
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
    /// The head commit's rollup, or `None` if the commit has no check suite.
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
    /// looser names a job that did nothing wrong. And only among the runs
    /// that still stand, or a superseded one names the wrong check and hands
    /// out a log link that 404s once its logs are collected.
    fn failing(&self) -> Option<FailedCheck> {
        let contexts = &self.rollup()?.contexts;
        let context = contexts
            .latest()
            .into_iter()
            .find(|context| context.failed())?;
        Some(FailedCheck {
            name: context.display_name()?.to_string(),
            job: context.job(),
        })
    }

    fn head(&self) -> Option<Head> {
        Some(Head {
            repo: self.head_repository.as_ref()?.name_with_owner.clone(),
            branch: self.head_ref_name.clone()?,
        })
    }

    fn actors(&self) -> Actors {
        let approved = self
            .latest_reviews
            .iter()
            .flat_map(|reviews| &reviews.nodes)
            .filter(|review| review.state == "APPROVED")
            .filter_map(|review| review.author.as_ref())
            .map(Reviewer::display)
            .collect();
        let draft = self
            .draft_toggles
            .as_ref()
            .and_then(|toggles| toggles.nodes.last())
            .and_then(|toggle| toggle.actor.as_ref())
            .map(Reviewer::display);
        let merged = self.merged_by.as_ref().map(Reviewer::display);
        Actors {
            approved,
            draft,
            merged,
            ..Actors::default()
        }
    }

    /// The latest comments and reviews, as who said something and when.
    ///
    /// A review counts its line comments, plus one for a body; one with
    /// neither, an approval with nothing to say, isn't a comment at all.
    fn discussion(&self) -> Discussion {
        let mut said = Discussion::default();
        let mut horizons = Vec::new();
        if let Some(comments) = &self.comments {
            said.total += comments.total_count;
            horizons.extend(comments.horizon(|c| Some(c.created_at)));
            said.remarks.extend(comments.nodes.iter().map(|comment| {
                Remark {
                    id: comment.id.clone(),
                    at: comment.created_at,
                    by: comment
                        .author
                        .as_ref()
                        .map_or(GHOST.into(), Reviewer::display),
                    weight: 1,
                    approval: false,
                }
            }));
        }
        if let Some(reviews) = &self.reviews {
            horizons.extend(reviews.horizon(|r| r.submitted_at));
            for review in &reviews.nodes {
                let Some(at) = review.submitted_at else {
                    continue;
                };
                let lines = review.comments.as_ref().map_or(0, |c| c.total_count);
                let weight = lines + u64::from(review.body);
                if weight == 0 {
                    continue;
                }
                said.total += weight;
                said.remarks.push(Remark {
                    id: review.id.clone(),
                    at,
                    by: review
                        .author
                        .as_ref()
                        .map_or(GHOST.into(), Reviewer::display),
                    weight,
                    approval: review.state == "APPROVED",
                });
            }
        }
        said.complete_after = horizons.into_iter().max();
        said
    }

    /// GitHub reports more rollup states than matter here. Anything not
    /// plainly finished reads as pending, so an unfamiliar one makes a wait
    /// keep waiting rather than declare a result it didn't get.
    fn checks(&self) -> Option<CheckState> {
        let rollup = self.rollup()?;
        if rollup.contexts.pruned
            && let Some(verdict) = rollup.contexts.verdict()
        {
            return Some(verdict);
        }
        Some(match rollup.state.as_str() {
            "SUCCESS" => CheckState::Success,
            // ERROR is a check that failed to run; for someone waiting to
            // respond, that is the same news as a failing one.
            //
            // GitHub's state counts every run ever recorded on this commit,
            // so it stays red for a run that a newer run of the same check
            // superseded — measured on a pull request that was green in
            // every other view (docs/watching.md). Red is believed only
            // while the runs that still stand back it, and whenever they
            // can't be read they can't disprove it either.
            "FAILURE" | "ERROR" => rollup.contexts.verdict().unwrap_or(CheckState::Failure),
            _ => CheckState::Pending,
        })
    }

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

    /// Recorded 2026-10-06 from buildkite/agent#4460: Buildkite reports as a
    /// commit status beside Actions check runs, one of which was re-run green.
    const BUILDKITE: &str = r#"{"state":"FAILURE","contexts":{"totalCount":4,"nodes":[
        {"__typename":"CheckRun","name":"Require category label","conclusion":"FAILURE",
         "databaseId":112556715016,"checkSuite":{"app":{"slug":"github-actions"},
         "workflowRun":{"event":"pull_request","workflow":{"name":"PR Labels"}}}},
        {"__typename":"CheckRun","name":"enable-auto-merge","conclusion":"SKIPPED",
         "databaseId":112556752234,"checkSuite":{"app":{"slug":"github-actions"},
         "workflowRun":{"event":"pull_request_target","workflow":{"name":"Dependabot auto-merge"}}}},
        {"__typename":"CheckRun","name":"Require category label","conclusion":"SUCCESS",
         "databaseId":112556872962,"checkSuite":{"app":{"slug":"github-actions"},
         "workflowRun":{"event":"pull_request","workflow":{"name":"PR Labels"}}}},
        {"__typename":"StatusContext","context":"buildkite/agent","state":"STATE"}]}}"#;

    fn buildkite(state: &str) -> RefStatus {
        let json = with_rollup(&BUILDKITE.replace("STATE", state));
        interpret(&[pr("agent", 4460)], &parse(&json))[0]
            .1
            .clone()
            .unwrap()
    }

    #[test]
    fn a_red_commit_status_is_named() {
        let status = buildkite("FAILURE");
        assert_eq!(status.checks, Some(CheckState::Failure));
        assert_eq!(
            status.failing,
            Some(FailedCheck {
                name: "buildkite/agent".into(),
                job: None,
            }),
            "not the label check, whose failure a re-run superseded"
        );
    }

    #[test]
    fn a_commit_status_still_running_reads_as_pending() {
        // GitHub's state is FAILURE only because of the superseded label run.
        for state in ["PENDING", "EXPECTED"] {
            assert_eq!(
                buildkite(state).checks,
                Some(CheckState::Pending),
                "{state}"
            );
        }
        assert_eq!(buildkite("SUCCESS").checks, Some(CheckState::Success));
    }

    fn ignoring(json: &str, names: &[&str]) -> RefStatus {
        let mut response = parse(json);
        response.ignore(&names.iter().map(|n| n.to_string()).collect::<Vec<_>>());
        interpret(&[pr("agent", 4460)], &response)[0]
            .1
            .clone()
            .unwrap()
    }

    #[test]
    fn an_ignored_check_neither_turns_a_rollup_red_nor_is_named() {
        let red = with_rollup(&BUILDKITE.replace("STATE", "FAILURE"));
        let status = ignoring(&red, &["buildkite/agent"]);
        assert_eq!(status.checks, Some(CheckState::Success));
        assert_eq!(status.failing, None);
    }

    #[test]
    fn an_ignored_check_still_running_does_not_hold_the_rollup_pending() {
        // A merge lock that never reports would otherwise keep every pull
        // request pending for as long as it is engaged.
        let json = with_rollup(&BUILDKITE.replace("STATE", "PENDING").replacen(
            r#""state":"FAILURE""#,
            r#""state":"PENDING""#,
            1,
        ));
        assert_eq!(
            ignoring(&json, &["buildkite/agent"]).checks,
            Some(CheckState::Success)
        );
    }

    #[test]
    fn ignoring_a_check_that_is_not_there_changes_nothing() {
        let red = with_rollup(&BUILDKITE.replace("STATE", "FAILURE"));
        assert_eq!(
            ignoring(&red, &["merge-lock"]).checks,
            Some(CheckState::Failure)
        );
    }

    #[test]
    fn an_unselected_member_of_the_union_leaves_the_rollup_unnamed() {
        let json = with_rollup(
            r#"{"state":"FAILURE","contexts":{"nodes":[
            {"__typename":"SomethingNew"}]}}"#,
        );
        let status = interpret(&[pr("x", 1)], &parse(&json))[0]
            .1
            .clone()
            .unwrap();
        assert_eq!(status.checks, Some(CheckState::Failure));
        assert_eq!(status.failing, None);
    }

    /// Recorded 2026-09-22 from dpep/inkling-ci-probe#8, whose in-flight run
    /// was superseded on the same head commit (the pull request was reopened
    /// under a `cancel-in-progress` concurrency group) and whose replacement
    /// then passed. Both runs of `slowish` stay in the rollup — the cancelled
    /// one keeps its conclusion forever — and GitHub's own `state` reads
    /// `FAILURE` for a pull request `gh pr checks` calls `pass` and the merge
    /// box calls `clean`. `later` is the conclusion of the surviving run.
    fn superseded(later: &str) -> String {
        let conclusion = if later == "null" {
            "null".to_string()
        } else {
            format!("\"{later}\"")
        };
        let suite = r#""checkSuite":{"app":{"slug":"github-actions"},
            "workflowRun":{"event":"pull_request","workflow":{"name":"supersede"}}}"#;
        with_rollup(&format!(
            r#"{{"state":"FAILURE","contexts":{{"totalCount":2,"nodes":[
            {{"__typename":"CheckRun","name":"slowish","conclusion":"CANCELLED",
              "databaseId":107051027337,{suite}}},
            {{"__typename":"CheckRun","name":"slowish","conclusion":{conclusion},
              "databaseId":107051237708,{suite}}}]}}}}"#
        ))
    }

    #[test]
    fn a_failure_no_current_run_backs_is_not_reported() {
        let status = interpret(&[pr("inkling-ci-probe", 8)], &parse(&superseded("SUCCESS")))[0]
            .1
            .clone()
            .unwrap();
        assert_eq!(
            status.checks,
            Some(CheckState::Success),
            "GitHub's rollup state still says FAILURE, from a run the newer \
             one superseded"
        );
        assert_eq!(status.failing, None);
    }

    #[test]
    fn a_superseded_failure_reads_as_pending_while_its_replacement_runs() {
        // The same pull request four seconds in: nothing has failed and
        // nothing has finished, so there is nothing to act on yet.
        let status = interpret(&[pr("inkling-ci-probe", 8)], &parse(&superseded("null")))[0]
            .1
            .clone()
            .unwrap();
        assert_eq!(status.checks, Some(CheckState::Pending));
        assert_eq!(status.failing, None);
    }

    /// Recorded 2026-09-22 from dpep/inkling-ci-probe#10: the only run was
    /// cancelled and nothing replaced it. GitHub's rollup, `gh pr checks` and
    /// `mergeable_state` all call that red, so Inkling does too rather than
    /// growing a fourth check state for it.
    #[test]
    fn a_cancelled_run_with_nothing_after_it_is_a_failure() {
        let json = with_rollup(
            r#"{"state":"FAILURE","contexts":{"totalCount":1,"nodes":[
            {"__typename":"CheckRun","name":"lonely","conclusion":"CANCELLED",
             "databaseId":107051852127,"checkSuite":{"app":{"slug":"github-actions"},
             "workflowRun":{"event":"pull_request","workflow":{"name":"lonely"}}}}]}}"#,
        );
        let status = interpret(&[pr("inkling-ci-probe", 10)], &parse(&json))[0]
            .1
            .clone()
            .unwrap();
        assert_eq!(status.checks, Some(CheckState::Failure));
        assert_eq!(
            status.failing.map(|f| f.name).as_deref(),
            Some("lonely"),
            "and it is still terminal, so a wait ends instead of spinning"
        );
    }

    /// Two jobs that share a name are one check only if everything else about
    /// them matches too. Splitting too finely just keeps an extra entry;
    /// merging two real checks hides a failure, so the key errs wide.
    #[test]
    fn checks_that_only_share_a_name_are_not_the_same_check() {
        let red = |workflow: &str, event: &str, id: u64| {
            format!(
                r#"{{"__typename":"CheckRun","name":"test","conclusion":"FAILURE",
                "databaseId":{id},"checkSuite":{{"app":{{"slug":"github-actions"}},
                "workflowRun":{{"event":"{event}","workflow":{{"name":"{workflow}"}}}}}}}}"#
            )
        };
        let green = r#"{"__typename":"CheckRun","name":"test","conclusion":"SUCCESS",
            "databaseId":999,"checkSuite":{"app":{"slug":"github-actions"},
            "workflowRun":{"event":"pull_request","workflow":{"name":"ci"}}}}"#;
        for stale in [red("nightly", "pull_request", 1), red("ci", "push", 2)] {
            let json = with_rollup(&format!(
                r#"{{"state":"FAILURE","contexts":{{"totalCount":2,
                "nodes":[{stale},{green}]}}}}"#
            ));
            let status = interpret(&[pr("x", 1)], &parse(&json))[0]
                .1
                .clone()
                .unwrap();
            assert_eq!(status.checks, Some(CheckState::Failure), "{stale}");
        }
    }

    #[test]
    fn a_red_rollup_is_believed_when_more_checks_exist_than_one_page_holds() {
        // Measured on home-assistant/core, whose pull requests carry 35 to 48
        // contexts: the failing one can sit past the page, and calling that
        // green would be the expensive way to be wrong.
        let json = with_rollup(
            r#"{"state":"FAILURE","contexts":{"totalCount":48,"nodes":[
            {"__typename":"CheckRun","name":"test","conclusion":"SUCCESS",
             "databaseId":1,"checkSuite":{"app":{"slug":"github-actions"},
             "workflowRun":{"event":"pull_request","workflow":{"name":"ci"}}}}]}}"#,
        );
        let status = interpret(&[pr("x", 1)], &parse(&json))[0]
            .1
            .clone()
            .unwrap();
        assert_eq!(status.checks, Some(CheckState::Failure));
    }

    #[test]
    fn the_query_asks_for_the_head_commits_rollup_and_its_contexts() {
        let query = build_query(&[pr("gqls", 1)]);
        assert!(query.contains("commits(last: 1)"), "{query}");
        assert!(query.contains("statusCheckRollup"), "{query}");
        // Measured: this costs nothing extra — a 3-reference batch is one
        // rate-limit point with it or without it, and one point still at
        // `first: 100` returning 130 contexts, so the page is free.
        assert!(query.contains("contexts(first: 100)"), "{query}");
        // `totalCount` says when a page didn't hold them all.
        assert!(query.contains("totalCount"), "{query}");
        // So a red Actions job's log can be named, and so two runs of the
        // same check can be told apart; also free, measured the same way.
        assert!(
            query.contains(
                "databaseId checkSuite { app { slug } \
                 workflowRun { event workflow { name } } }"
            ),
            "{query}"
        );
        assert!(
            query.contains("... on StatusContext { context state }"),
            "{query}"
        );
    }

    #[test]
    fn a_broken_query_is_an_error_not_a_missing_pull_request() {
        // Recorded 2026-10-06 from a query with one brace too many.
        let json = r#"{"errors":[{"message":"Expected one of SCHEMA, SCALAR, TYPE, ENUM, INPUT, UNION, INTERFACE, actual: RCURLY (\"}\") at [1, 488]","locations":[{"line":1,"column":488}]}]}"#;
        let result = interpret(&[pr("x", 1)], &parse(json))[0].1.clone();
        assert!(matches!(result, Err(RefError::Other(_))), "{result:?}");
    }

    #[test]
    fn approvers_are_named_by_login_and_bots_say_so() {
        // Recorded 2026-10-06 from buildkite/agent#4456 and #4455: a review
        // bot approves beside a person, or only comments.
        let reviewed = |reviews: &str| {
            let json = format!(
                r#"{{"data":{{"r0":{{"pullRequest":{{"isDraft":false,"state":"OPEN",
                "reviewDecision":"APPROVED","latestReviews":{{"nodes":[{reviews}]}}}}}}}}}}"#
            );
            interpret(&[pr("agent", 4456)], &parse(&json))[0]
                .1
                .clone()
                .unwrap()
                .by
                .approved
        };
        assert_eq!(
            reviewed(
                r#"{"state":"APPROVED","author":{"__typename":"User","login":"CerealBoy"}},
                {"state":"APPROVED","author":{"__typename":"Bot","login":"buildsworth-bk-app"}}"#
            ),
            ["@CerealBoy", "buildsworth-bk-app (bot)"]
        );
        assert_eq!(
            reviewed(
                r#"{"state":"COMMENTED","author":{"__typename":"Bot","login":"buildsworth-bk-app"}},
                {"state":"APPROVED","author":{"__typename":"User","login":"CerealBoy"}},
                {"state":"APPROVED","author":null}"#
            ),
            ["@CerealBoy"],
            "a comment is not an approval, and a deleted account has no name"
        );
    }

    #[test]
    fn the_latest_draft_toggle_names_who_made_it() {
        // Recorded 2026-10-06 from dpep/inkling#25.
        let json = r#"{"data":{"r0":{"pullRequest":{"isDraft":false,"state":"OPEN",
            "reviewDecision":null,"timelineItems":{"nodes":[
            {"__typename":"ReadyForReviewEvent","actor":{"__typename":"User","login":"dpep"}}]}}}}}"#;
        let status = interpret(&[pr("inkling", 25)], &parse(json))[0]
            .1
            .clone()
            .unwrap();
        assert_eq!(status.by.draft.as_deref(), Some("@dpep"));
    }

    #[test]
    fn a_merge_names_who_merged_it() {
        // Recorded 2026-10-10 from dpep/inkling#29.
        let json = r#"{"data":{"r0":{"pullRequest":{"isDraft":false,"state":"MERGED",
            "reviewDecision":null,"mergedBy":{"__typename":"User","login":"dpep"}}}}}"#;
        let status = interpret(&[pr("inkling", 29)], &parse(json))[0]
            .1
            .clone()
            .unwrap();
        assert_eq!(status.by.merged.as_deref(), Some("@dpep"));
    }

    #[test]
    fn the_query_is_balanced() {
        // A stray brace is a GraphQL error for the whole batch, which reads as
        // every reference vanishing rather than as a broken query.
        let query = build_query(&[pr("a", 1), pr("b", 2)]);
        let mut depth = 0i32;
        for c in query.chars() {
            depth += match c {
                '{' | '(' => 1,
                '}' | ')' => -1,
                _ => 0,
            };
            assert!(depth >= 0, "closes more than it opens: {query}");
        }
        assert_eq!(depth, 0, "{query}");
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

    /// Recorded 2026-11-17 from home-assistant/core#193001 with the query as
    /// built: bots and people commenting, copilot reviewing with a body, two
    /// approvals with none. Review bodies are masked to `…` or left empty,
    /// which is all a reading keeps of them.
    const HOME_ASSISTANT: &str = r#"{"data":{"viewer":{"login":"dpep"},"r0":{"pullRequest":{"isDraft":false,"state":"MERGED","reviewDecision":"APPROVED","comments":{"totalCount":5,"nodes":[{"id":"IC_kwDOAMSroc8AAAABnRRzZg","createdAt":"2026-11-16T21:45:23Z","author":{"__typename":"Bot","login":"home-assistant"}},{"id":"IC_kwDOAMSroc8AAAABnRR5qw","createdAt":"2026-11-16T21:45:27Z","author":{"__typename":"Bot","login":"home-assistant"}},{"id":"IC_kwDOAMSroc8AAAABnRb_mQ","createdAt":"2026-11-16T21:52:43Z","author":{"__typename":"Bot","login":"github-actions"}},{"id":"IC_kwDOAMSroc8AAAABnRfoJQ","createdAt":"2026-11-16T21:55:24Z","author":{"__typename":"Bot","login":"github-actions"}},{"id":"IC_kwDOAMSroc8AAAABnbWhog","createdAt":"2026-11-17T06:44:26Z","author":{"__typename":"User","login":"OnFreund"}}]},"reviews":{"totalCount":5,"nodes":[{"id":"PRR_kwDOAMSroc8AAAABYVqeVg","state":"COMMENTED","submittedAt":"2026-11-16T21:45:27Z","body":"…","author":{"__typename":"Bot","login":"copilot-pull-request-reviewer"},"comments":{"totalCount":0}},{"id":"PRR_kwDOAMSroc8AAAABYbOc8g","state":"APPROVED","submittedAt":"2026-11-17T06:44:52Z","body":"","author":{"__typename":"User","login":"OnFreund"},"comments":{"totalCount":0}},{"id":"PRR_kwDOAMSroc8AAAABYmF1VQ","state":"COMMENTED","submittedAt":"2026-11-17T19:53:09Z","body":"…","author":{"__typename":"Bot","login":"copilot-pull-request-reviewer"},"comments":{"totalCount":0}},{"id":"PRR_kwDOAMSroc8AAAABYmI5Iw","state":"COMMENTED","submittedAt":"2026-11-17T19:56:20Z","body":"…","author":{"__typename":"Bot","login":"copilot-pull-request-reviewer"},"comments":{"totalCount":0}},{"id":"PRR_kwDOAMSroc8AAAABYmJ8xg","state":"APPROVED","submittedAt":"2026-11-17T19:57:35Z","body":"","author":{"__typename":"User","login":"silamon"},"comments":{"totalCount":0}}]}}}}}"#;

    fn discussion(json: &str) -> (Discussion, Option<String>) {
        let status = interpret(&[pr("x", 1)], &parse(json))[0].1.clone().unwrap();
        (status.discussion.unwrap(), status.by.me)
    }

    #[test]
    fn a_reading_counts_comments_and_reviews_that_say_something() {
        let (said, me) = discussion(HOME_ASSISTANT);
        assert_eq!(
            me.as_deref(),
            Some("@dpep"),
            "the viewer, spelled as an actor"
        );
        assert_eq!(said.total, 8, "five comments and copilot's three reviews");
        assert_eq!(said.complete_after, None, "both pages held everything");
        let who: Vec<&str> = said.remarks.iter().map(|r| r.by.as_str()).collect();
        assert_eq!(
            who,
            [
                "home-assistant (bot)",
                "home-assistant (bot)",
                "github-actions (bot)",
                "github-actions (bot)",
                "@OnFreund",
                "copilot-pull-request-reviewer (bot)",
                "copilot-pull-request-reviewer (bot)",
                "copilot-pull-request-reviewer (bot)",
            ],
            "an approval with nothing to say is not a comment"
        );
    }

    /// Recorded 2026-11-17 from ohcnetwork/care_fe#16618: 125 reviews, so
    /// the page holds the latest 30; trimmed here to five of them. One is a
    /// lint bot's 50 line comments under an empty review, one a person
    /// requesting changes with a body and three line comments.
    const CARE_FE: &str = r#"{"data":{"viewer":{"login":"dpep"},"r0":{"pullRequest":{"isDraft":false,"state":"OPEN","reviewDecision":"CHANGES_REQUESTED","comments":{"totalCount":11,"nodes":[{"id":"IC_kwDODtfaXs8AAAABMrSJ-A","createdAt":"2026-07-31T17:22:27Z","author":{"__typename":"Bot","login":"github-actions"}},{"id":"IC_kwDODtfaXs8AAAABMrSbvg","createdAt":"2026-07-31T17:22:48Z","author":{"__typename":"Bot","login":"coderabbitai"}},{"id":"IC_kwDODtfaXs8AAAABMrdMDQ","createdAt":"2026-07-31T17:44:20Z","author":{"__typename":"Bot","login":"github-actions"}},{"id":"IC_kwDODtfaXs8AAAABMs4dQw","createdAt":"2026-07-31T20:49:25Z","author":{"__typename":"Bot","login":"cloudflare-workers-and-pages"}},{"id":"IC_kwDODtfaXs8AAAABNIGt6Q","createdAt":"2026-08-04T07:25:41Z","author":{"__typename":"Bot","login":"github-actions"}},{"id":"IC_kwDODtfaXs8AAAABNuXqIA","createdAt":"2026-08-07T10:42:30Z","author":{"__typename":"User","login":"nihal467"}},{"id":"IC_kwDODtfaXs8AAAABSgCgNg","createdAt":"2026-09-04T06:16:32Z","author":{"__typename":"Bot","login":"copilot-pull-request-reviewer"}},{"id":"IC_kwDODtfaXs8AAAABSg9ePA","createdAt":"2026-09-04T07:56:31Z","author":{"__typename":"Bot","login":"copilot-pull-request-reviewer"}},{"id":"IC_kwDODtfaXs8AAAABSz4IUw","createdAt":"2026-09-06T05:59:35Z","author":{"__typename":"Bot","login":"github-actions"}},{"id":"IC_kwDODtfaXs8AAAABUmxekg","createdAt":"2026-09-15T09:21:12Z","author":{"__typename":"Bot","login":"github-actions"}},{"id":"IC_kwDODtfaXs8AAAABXwa3Gw","createdAt":"2026-09-29T11:22:22Z","author":{"__typename":"Bot","login":"github-actions"}}]},"reviews":{"totalCount":125,"nodes":[{"id":"PRR_kwDODtfaXs8AAAABPjl4SQ","state":"COMMENTED","submittedAt":"2026-09-28T12:55:48Z","body":"","author":{"__typename":"Bot","login":"github-actions"},"comments":{"totalCount":50}},{"id":"PRR_kwDODtfaXs8AAAABRD49JA","state":"COMMENTED","submittedAt":"2026-10-07T08:52:20Z","body":"…","author":{"__typename":"Bot","login":"copilot-pull-request-reviewer"},"comments":{"totalCount":1}},{"id":"PRR_kwDODtfaXs8AAAABRgWsPA","state":"COMMENTED","submittedAt":"2026-10-09T12:02:25Z","body":"…","author":{"__typename":"Bot","login":"copilot-pull-request-reviewer"},"comments":{"totalCount":2}},{"id":"PRR_kwDODtfaXs8AAAABRrygeQ","state":"COMMENTED","submittedAt":"2026-10-11T03:28:29Z","body":"…","author":{"__typename":"Bot","login":"copilot-pull-request-reviewer"},"comments":{"totalCount":0}},{"id":"PRR_kwDODtfaXs8AAAABSpyv4w","state":"CHANGES_REQUESTED","submittedAt":"2026-10-22T08:18:06Z","body":"…","author":{"__typename":"User","login":"yash-learner"},"comments":{"totalCount":3}}]}}}}}"#;

    #[test]
    fn a_review_counts_its_line_comments_and_its_body() {
        let (said, _) = discussion(CARE_FE);
        let weights: Vec<(&str, u64, bool)> = said.remarks[11..]
            .iter()
            .map(|r| (r.by.as_str(), r.weight, r.approval))
            .collect();
        assert_eq!(
            weights,
            [
                ("github-actions (bot)", 50, false),
                ("copilot-pull-request-reviewer (bot)", 2, false),
                ("copilot-pull-request-reviewer (bot)", 3, false),
                ("copilot-pull-request-reviewer (bot)", 1, false),
                ("@yash-learner", 4, false),
            ]
        );
        assert_eq!(said.total, 11 + 50 + 2 + 3 + 1 + 4);
    }

    #[test]
    fn more_reviews_than_a_page_marks_where_the_reading_stops() {
        let (said, _) = discussion(CARE_FE);
        assert_eq!(
            said.complete_after,
            Some("2026-09-28T12:55:48Z".parse().unwrap()),
            "the oldest review on the page; the comments page held all eleven"
        );
    }

    #[test]
    fn the_query_asks_for_the_latest_comments_and_who_is_asking() {
        let query = build_query(&[pr("a", 1), pr("b", 2)]);
        assert_eq!(query.matches("viewer { login }").count(), 1, "{query}");
        assert!(query.contains("comments(last: 30) { totalCount"), "{query}");
        assert!(
            query.contains("reviews(last: 30) { totalCount nodes { id state submittedAt body"),
            "{query}"
        );
    }

    #[test]
    fn a_pending_review_or_a_deleted_account_does_not_break_a_reading() {
        let json = r#"{"data":{"r0":{"pullRequest":{"isDraft":false,"state":"OPEN","reviewDecision":null,
            "comments":{"totalCount":1,"nodes":[{"id":"IC_1","createdAt":"2026-11-17T10:00:00Z","author":null}]},
            "reviews":{"totalCount":1,"nodes":[{"id":"PRR_1","state":"PENDING","submittedAt":null,"body":"…",
            "author":{"__typename":"User","login":"dpep"},"comments":{"totalCount":2}}]}}}}}"#;
        let (said, me) = discussion(json);
        assert_eq!(
            me, None,
            "no viewer in the response, so nobody is the reader"
        );
        assert_eq!(said.total, 1, "a pending review is unpublished");
        assert_eq!(said.remarks[0].by, "@ghost");
    }
}
