//! Finding the external things a Slack message points at.
//!
//! The hard half — pulling URLs out of prose and parsing them into parts — is
//! `iriq`'s. What is left is a structural match on the path shape, which is
//! why repo roots, `/issues/7`, and `/commit/<sha>` need no rejection rules:
//! they simply don't match.

use crate::domain::model::Reference;
use iriq::Extractor;

/// What a positional argument to `run` turned out to mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A fully-qualified pull request.
    Reference(Reference),
    /// A reference missing some of its parts — `inkling#2`, `dpep/inkling#2`,
    /// or a bare `2`. Ambiguous until matched against tracked records;
    /// guessing the rest would react to the wrong pull request.
    Partial {
        owner: Option<String>,
        repo: Option<String>,
        number: u64,
    },
    Channel(String),
}

/// Classify one `run` argument.
///
/// Order matters: a URL is unambiguous and wins, then the short form Inkling
/// prints in its own output, then a bare number, then anything else is a
/// channel.
///
/// An argument that plainly means a pull request but doesn't parse is an
/// error rather than a channel — `github.com/dpep/inklng#2` is a typo, not a
/// channel somebody named that.
pub fn parse_target(arg: &str, hosts: &[String]) -> Result<Target, String> {
    let trimmed = arg.trim();

    if let Some(reference) = detect(trimmed, hosts).into_iter().next() {
        return Ok(Target::Reference(reference));
    }
    if let Some(reference) = parse_short_ref(trimmed, hosts) {
        return Ok(Target::Reference(reference));
    }
    if let Some(reference) = parse_jira(trimmed) {
        return Ok(Target::Reference(reference));
    }

    if let Some(partial) = parse_partial(trimmed) {
        return Ok(partial);
    }

    if looks_like_a_pull_request(trimmed) {
        return Err(format!(
            "{trimmed}: looks like a pull request but isn't one — expected \
             https://<host>/<owner>/<repo>/pull/<number>"
        ));
    }
    if looks_like_a_jira_issue(trimmed) {
        return Err(format!(
            "{trimmed}: looks like a Jira issue but isn't one — expected a key \
             like AI-123, or an .atlassian.net/browse/ URL"
        ));
    }
    Ok(Target::Channel(trimmed.to_string()))
}

/// `AI-123`, or the browse URL Jira puts in the address bar.
///
/// Uppercase only, which is how Jira itself writes keys — accepting `eng-1`
/// would swallow channel handles of the same shape.
fn parse_jira(arg: &str) -> Option<Reference> {
    let key = match arg.split_once("/browse/") {
        Some((site, tail)) if site.to_lowercase().contains("atlassian.net") => {
            tail.split(['/', '?', '#']).next()?
        }
        Some(_) => return None,
        None => arg,
    };
    is_issue_key(key).then(|| Reference::JiraIssue {
        key: key.to_string(),
    })
}

fn is_issue_key(key: &str) -> bool {
    let Some((project, number)) = key.split_once('-') else {
        return false;
    };
    project.starts_with(|c: char| c.is_ascii_uppercase())
        && project
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        && !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit())
}

/// `github.com/owner/repo#12` — the form Inkling prints, accepted back.
fn parse_short_ref(arg: &str, hosts: &[String]) -> Option<Reference> {
    let (path, number) = arg.rsplit_once('#')?;
    let number: u64 = number.parse().ok()?;
    let parts: Vec<&str> = path.split('/').collect();
    let [host, owner, repo] = parts.as_slice() else {
        return None;
    };
    let host = host.to_lowercase();
    hosts
        .iter()
        .any(|h| h.to_lowercase() == host)
        .then(|| Reference::GitHubPullRequest {
            host,
            owner: (*owner).to_string(),
            repo: (*repo).to_string(),
            number,
        })
}

/// `2`, `#2`, `inkling#2`, or `dpep/inkling#2` — everything short of a host.
fn parse_partial(arg: &str) -> Option<Target> {
    let (prefix, digits) = match arg.rsplit_once('#') {
        Some((prefix, digits)) => (prefix, digits),
        // A bare number, with no `#` to split on.
        None => ("", arg),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let number: u64 = digits.parse().ok()?;

    let (owner, repo) = match prefix.split('/').collect::<Vec<_>>().as_slice() {
        [""] => (None, None),
        [repo] => (None, Some((*repo).to_string())),
        [owner, repo] => (Some((*owner).to_string()), Some((*repo).to_string())),
        // Three parts means a host, which `parse_short_ref` already handled;
        // reaching here means its host wasn't configured.
        _ => return None,
    };
    Some(Target::Partial {
        owner,
        repo,
        number,
    })
}

/// Enough of a pull-request shape that treating it as a channel would hide a
/// typo rather than do what was asked.
fn looks_like_a_pull_request(arg: &str) -> bool {
    let lower = arg.to_lowercase();
    lower.contains("/pull/") || lower.contains("github.")
}

fn looks_like_a_jira_issue(arg: &str) -> bool {
    let lower = arg.to_lowercase();
    lower.contains("/browse/") || lower.contains("atlassian.net")
}

/// Extract every distinct reference in `text`, in the order they appear.
///
/// `hosts` is the set of GitHub hosts to honor, so an enterprise install can
/// be recognized without treating every lookalike domain as GitHub.
pub fn detect(text: &str, hosts: &[String]) -> Vec<Reference> {
    let unwrapped = unwrap_slack_links(text);
    let mut found = Vec::new();
    for iri in Extractor::new().extract(&unwrapped) {
        if let Some(reference) = as_pull_request(&iri, hosts)
            && !found.contains(&reference)
        {
            found.push(reference);
        }
    }
    found
}

/// Commands whose text or output names a pull request the caller is working
/// on. Not `gh pr list` or `gh pr status`: those name everybody's.
const PULL_COMMANDS: [&str; 7] = [
    "gh pr create",
    "gh pr view",
    "gh pr checks",
    "gh pr ready",
    "gh pr merge",
    "gh pr edit",
    "gh pr comment",
];

const ISSUE_COMMAND: &str = "acli jira workitem";
const BROWSE: &str = "atlassian.net/browse/";

/// Enough output to find a reference in, and no more.
///
/// A command that prints a pull request's URL prints it early — `gh pr
/// create` prints it alone, `git push` in its remote block. Reading a whole
/// megabyte of somebody's test output to find that out would put the cost of
/// a rare case on every tool call.
const OUTPUT_LIMIT: usize = 64 * 1024;

/// How many references one command may be about.
///
/// `gh pr create` prints one, `git push` prints one, and a command that names
/// a dozen pull requests is not a command about any of them. Without a cap,
/// 64KiB of URLs is ~1,600 of them: measured 2026-09-20, one PostToolUse
/// registered 1,586 watches, which the next poll turned into a 481KiB GraphQL
/// query that GitHub will refuse — and one refused batch is every watch on the
/// machine going unread, because a poll asks about all of them at once.
const MAX_REFERENCES: usize = 8;

/// Whether a command is worth reading at all.
///
/// The gate, and the reason a hook on every tool call is affordable: a few
/// substring scans over the command itself. Anything that isn't plainly about
/// a pull request or an issue stops here, before any parsing and before the
/// store is opened.
pub fn worth_reading(command: &str) -> bool {
    command.contains("git push")
        || command.contains(BROWSE)
        || command.contains(ISSUE_COMMAND)
        || PULL_COMMANDS.iter().any(|verb| command.contains(verb))
}

/// What a shell command was about, and what makes us think so.
///
/// Narrow on purpose, because the cost of being wrong is a session told about
/// something it never asked for. A reference is taken from a URL, which is
/// unambiguous, or from a Jira key that was an *argument* to `acli`. Never
/// from a key-shaped word in arbitrary output: `UTF-8`, `SHA-256` and
/// `CVE-2024` are all issue keys as far as the shape goes.
pub fn touched(command: &str, output: &str, hosts: &[String]) -> Vec<(Reference, &'static str)> {
    // On a character boundary, so a cap in the middle of a multi-byte glyph
    // shortens the slice rather than panicking.
    let limit = (0..=OUTPUT_LIMIT.min(output.len()))
        .rev()
        .find(|n| output.is_char_boundary(*n))
        .unwrap_or(0);
    let output = &output[..limit];
    let mut found: Vec<(Reference, &'static str)> = Vec::new();
    let mut add = |reference: Reference, why: &'static str| {
        if found.len() < MAX_REFERENCES && !found.iter().any(|(seen, _)| *seen == reference) {
            found.push((reference, why));
        }
    };

    if let Some(verb) = PULL_COMMANDS.iter().find(|verb| command.contains(**verb)) {
        for reference in detect(command, hosts)
            .into_iter()
            .chain(detect(output, hosts))
        {
            add(reference, verb);
        }
    }
    if command.contains("git push") {
        // Only the output: a push command never carries the pull request's
        // URL, and some remotes print it. When none does there is nothing to
        // watch, and nothing here goes and looks one up — a hook runs on
        // every tool call, and a network round trip is not something to spend
        // there.
        for reference in detect(output, hosts) {
            add(reference, "git push");
        }
    }
    if let Some((_, arguments)) = command.split_once(ISSUE_COMMAND) {
        for word in arguments.split(|c: char| c.is_whitespace() || c == '=') {
            if let Some(reference) = parse_jira(word) {
                add(reference, ISSUE_COMMAND);
            }
        }
    }
    for word in command.split_whitespace() {
        if word.contains(BROWSE)
            && let Some(reference) = parse_jira(word)
        {
            add(reference, "browse url");
        }
    }
    found
}

fn as_pull_request(iri: &iriq::Identifier, hosts: &[String]) -> Option<Reference> {
    let host = iri.host.to_lowercase();
    if !hosts.iter().any(|h| h.to_lowercase() == host) {
        return None;
    }
    match iri.path_segments.as_slice() {
        [owner, repo, kind, number, ..] if kind.as_str() == "pull" => {
            Some(Reference::GitHubPullRequest {
                host,
                owner: owner.clone(),
                repo: repo.clone(),
                number: number.parse().ok()?,
            })
        }
        _ => None,
    }
}

/// Slack wraps links as `<url>` or `<url|label>`. Left alone, the label
/// leaks into the parsed URL — `…/pull/42|PR 42` — and the number stops
/// parsing. `|` is not legal unescaped in a URL, so cutting there is safe.
fn unwrap_slack_links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('>') {
            Some(close) => {
                let inner = &after[..close];
                let url = inner.split('|').next().unwrap_or(inner);
                // Padded so an unwrapped link can't glue itself to whatever
                // sat against the angle bracket.
                out.push(' ');
                out.push_str(url);
                out.push(' ');
                rest = &after[close + 1..];
            }
            None => {
                out.push('<');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts() -> Vec<String> {
        vec!["github.com".to_string()]
    }

    fn pr(owner: &str, repo: &str, number: u64) -> Reference {
        Reference::GitHubPullRequest {
            host: "github.com".to_string(),
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
        }
    }

    fn detected(text: &str) -> Vec<Reference> {
        detect(text, &hosts())
    }

    #[test]
    fn finds_a_plain_pull_request_url() {
        assert_eq!(
            detected("https://github.com/dpep/inkling/pull/42"),
            vec![pr("dpep", "inkling", 42)]
        );
    }

    #[test]
    fn unwraps_slack_link_syntax() {
        // The label used to leak in as part of the PR number.
        assert_eq!(
            detected("ready for review <https://github.com/dpep/inkling/pull/42|PR 42>"),
            vec![pr("dpep", "inkling", 42)]
        );
        assert_eq!(
            detected("<https://github.com/dpep/inkling/pull/42>"),
            vec![pr("dpep", "inkling", 42)]
        );
    }

    #[test]
    fn tolerates_an_unclosed_angle_bracket() {
        assert_eq!(
            detected("2 < 3 and https://github.com/dpep/inkling/pull/7"),
            vec![pr("dpep", "inkling", 7)]
        );
    }

    #[test]
    fn ignores_trailing_path_fragment_and_query() {
        for suffix in [
            "/files",
            "#discussion_r123",
            "?w=1",
            "/files#diff-abc",
            "/commits",
        ] {
            let text = format!("https://github.com/dpep/inkling/pull/43{suffix}");
            assert_eq!(detected(&text), vec![pr("dpep", "inkling", 43)], "{suffix}");
        }
    }

    #[test]
    fn ignores_punctuation_against_the_url() {
        assert_eq!(
            detected("see https://github.com/dpep/inkling/pull/9."),
            vec![pr("dpep", "inkling", 9)]
        );
        assert_eq!(
            detected("(https://github.com/dpep/inkling/pull/9)"),
            vec![pr("dpep", "inkling", 9)]
        );
    }

    #[test]
    fn rejects_links_that_are_not_pull_requests() {
        for url in [
            "https://github.com/dpep/inkling",
            "https://github.com/dpep/inkling/issues/7",
            "https://github.com/dpep/inkling/commit/abc123",
            "https://github.com/dpep/inkling/pulls",
            "https://github.com/dpep",
            "https://github.com/dpep/inkling/pull/not-a-number",
            "https://example.com/dpep/inkling/pull/42",
        ] {
            assert!(detected(url).is_empty(), "should not match: {url}");
        }
    }

    #[test]
    fn rejects_a_lookalike_host() {
        assert!(detected("https://github.com.evil.example/dpep/inkling/pull/42").is_empty());
        assert!(detected("https://notgithub.com/dpep/inkling/pull/42").is_empty());
    }

    #[test]
    fn honors_a_configured_enterprise_host() {
        let hosts = vec!["github.example.com".to_string()];
        let found = detect("https://github.example.com/team/svc/pull/5", &hosts);
        assert_eq!(
            found,
            vec![Reference::GitHubPullRequest {
                host: "github.example.com".to_string(),
                owner: "team".to_string(),
                repo: "svc".to_string(),
                number: 5,
            }]
        );
    }

    #[test]
    fn finds_several_references_in_order() {
        let text = "stack: https://github.com/dpep/inkling/pull/1 then \
                    <https://github.com/dpep/inkling/pull/2|part two>";
        assert_eq!(
            detected(text),
            vec![pr("dpep", "inkling", 1), pr("dpep", "inkling", 2)]
        );
    }

    #[test]
    fn deduplicates_repeats_of_the_same_pull_request() {
        let text = "https://github.com/dpep/inkling/pull/3 and again \
                    https://github.com/dpep/inkling/pull/3/files";
        assert_eq!(detected(text), vec![pr("dpep", "inkling", 3)]);
    }

    fn target(arg: &str) -> Result<Target, String> {
        parse_target(arg, &hosts())
    }

    #[test]
    fn a_url_argument_is_a_pull_request() {
        assert_eq!(
            target("https://github.com/dpep/inkling/pull/2").unwrap(),
            Target::Reference(pr("dpep", "inkling", 2))
        );
        // Scheme optional, trailing path ignored.
        assert_eq!(
            target("github.com/dpep/inkling/pull/2/files").unwrap(),
            Target::Reference(pr("dpep", "inkling", 2))
        );
    }

    #[test]
    fn the_short_form_inkling_prints_is_accepted_back() {
        // Whatever `refs=` shows in the output should work as input.
        assert_eq!(
            target("github.com/dpep/inkling#2").unwrap(),
            Target::Reference(pr("dpep", "inkling", 2))
        );
    }

    fn partial(owner: Option<&str>, repo: Option<&str>, number: u64) -> Target {
        Target::Partial {
            owner: owner.map(String::from),
            repo: repo.map(String::from),
            number,
        }
    }

    #[test]
    fn shorter_forms_stay_ambiguous_for_the_caller_to_resolve() {
        assert_eq!(target("365304").unwrap(), partial(None, None, 365304));
        assert_eq!(target("#2").unwrap(), partial(None, None, 2));
        assert_eq!(
            target("inkling#2").unwrap(),
            partial(None, Some("inkling"), 2)
        );
        assert_eq!(
            target("dpep/inkling#2").unwrap(),
            partial(Some("dpep"), Some("inkling"), 2)
        );
    }

    #[test]
    fn channel_handles_still_parse_as_channels() {
        assert_eq!(target("#eng").unwrap(), Target::Channel("#eng".into()));
        assert_eq!(
            target("eng-prs").unwrap(),
            Target::Channel("eng-prs".into())
        );
        assert_eq!(
            target("C012XYZ6789").unwrap(),
            Target::Channel("C012XYZ6789".into())
        );
    }

    fn issue(key: &str) -> Target {
        Target::Reference(Reference::JiraIssue {
            key: key.to_string(),
        })
    }

    #[test]
    fn a_jira_key_is_a_reference() {
        assert_eq!(target("AI-123").unwrap(), issue("AI-123"));
        assert_eq!(target("ABC2-7").unwrap(), issue("ABC2-7"));
    }

    #[test]
    fn a_browse_url_reduces_to_its_key() {
        for url in [
            "https://example.atlassian.net/browse/AI-1",
            "example.atlassian.net/browse/AI-1",
            "https://example.atlassian.net/browse/AI-1?focusedCommentId=9",
            "https://example.atlassian.net/browse/AI-1#comment",
        ] {
            assert_eq!(target(url).unwrap(), issue("AI-1"), "{url}");
        }
    }

    #[test]
    fn lowercase_lookalikes_stay_channels() {
        // Jira writes keys uppercase; accepting `eng-1` would swallow every
        // channel handle of that shape.
        assert_eq!(target("eng-1").unwrap(), Target::Channel("eng-1".into()));
        assert_eq!(target("AI").unwrap(), Target::Channel("AI".into()));
        assert_eq!(target("AI-").unwrap(), Target::Channel("AI-".into()));
    }

    #[test]
    fn a_malformed_jira_url_errors_rather_than_becoming_a_channel() {
        assert!(target("https://example.atlassian.net/browse/nonsense").is_err());
        assert!(target("https://example.atlassian.net/jira/software").is_err());
    }

    #[test]
    fn jira_keys_are_not_detected_in_slack_prose() {
        // Watching a ticket is opt-in. Left in `detect`, every "AI-123" in a
        // message would become a tracked record the reaction loop must map to
        // an emoji — a product decision nobody made.
        assert!(detected("AI-123 is next").is_empty());
        assert!(detected("https://example.atlassian.net/browse/AI-1").is_empty());
    }

    #[test]
    fn a_malformed_pull_request_errors_rather_than_becoming_a_channel() {
        // Far more likely a typo than a channel actually named this, and
        // treating it as one would silently search for nothing.
        assert!(target("github.com/dpep/inkling#").is_err());
        assert!(target("https://github.com/dpep/pull/2").is_err());
        assert!(target("github.com/dpep/inkling/pull/abc").is_err());
    }

    #[test]
    fn an_unconfigured_host_is_not_silently_a_channel() {
        assert!(parse_target("github.enterprise.io/a/b#1", &hosts()).is_err());
    }

    #[test]
    fn finds_nothing_in_text_without_links() {
        assert!(detected("just a normal message about a pull request").is_empty());
        assert!(detected("").is_empty());
    }
}

#[cfg(test)]
mod touched_tests {
    use super::*;

    fn hosts() -> Vec<String> {
        vec!["github.com".to_string()]
    }

    fn names(command: &str, output: &str) -> Vec<String> {
        touched(command, output, &hosts())
            .into_iter()
            .map(|(reference, _)| reference.to_string())
            .collect()
    }

    #[test]
    fn the_gate_turns_away_everything_that_is_not_plainly_about_one() {
        // The property that makes a hook on every tool call affordable.
        for command in [
            "cargo test",
            "rg -n 'pull' src/",
            "gh pr list --limit 5",
            "git status",
            "echo https://github.com/dpep/inkling/pull/7",
        ] {
            assert!(!worth_reading(command), "{command}");
        }
        for command in [
            "gh pr create --draft",
            "git push -u origin HEAD",
            "acli jira workitem view --key AI-1",
            "open https://x.atlassian.net/browse/AI-1",
        ] {
            assert!(worth_reading(command), "{command}");
        }
    }

    #[test]
    fn a_pull_request_command_is_read_from_its_arguments_and_its_output() {
        assert_eq!(
            names("gh pr checks https://github.com/dpep/inkling/pull/7", ""),
            ["github.com/dpep/inkling#7"]
        );
        assert_eq!(
            names(
                "gh pr create --fill",
                "https://github.com/dpep/inkling/pull/8\n"
            ),
            ["github.com/dpep/inkling#8"]
        );
    }

    #[test]
    fn a_push_is_read_from_its_output_only() {
        // The command never carries the URL; some remotes print it.
        assert_eq!(
            names(
                "git push -u origin HEAD",
                "remote: Create a pull request by visiting:\n\
                 remote:   https://github.com/dpep/inkling/pull/9\n"
            ),
            ["github.com/dpep/inkling#9"]
        );
        assert!(
            names("git push -u origin HEAD", "").is_empty(),
            "and nothing goes looking for one"
        );
    }

    #[test]
    fn the_branch_link_a_push_usually_prints_is_not_a_pull_request() {
        // `/pull/new/<branch>` is an invitation to open one, not one that
        // exists. Watching it would watch a number that never was.
        assert!(
            names(
                "git push -u origin HEAD",
                "remote:   https://github.com/dpep/inkling/pull/new/subscribe-push\n"
            )
            .is_empty()
        );
    }

    #[test]
    fn a_jira_key_counts_only_where_it_was_an_argument() {
        assert_eq!(
            names("acli jira workitem view --key AI-123", ""),
            ["AI-123"]
        );
        assert_eq!(names("acli jira workitem view AI-7", ""), ["AI-7"]);
    }

    #[test]
    fn key_shaped_words_in_output_are_not_issues() {
        // The reason output is never scanned for bare keys: plenty of things
        // are shaped like one.
        let noise = "UTF-8 SHA-256 CVE-2024 HTTP-2 AI-1";
        assert!(names("acli jira workitem view --key AI-9", noise).contains(&"AI-9".to_string()));
        assert_eq!(
            names("acli jira workitem view --key AI-9", noise).len(),
            1,
            "and only the argument"
        );
    }

    #[test]
    fn a_browse_url_is_unambiguous_wherever_it_sits() {
        assert_eq!(
            names("open https://dpep.atlassian.net/browse/AI-42", ""),
            ["AI-42"]
        );
    }

    #[test]
    fn what_matched_is_reported_so_the_log_can_say_why() {
        let why: Vec<&str> = touched(
            "gh pr checks 7 https://github.com/dpep/inkling/pull/7",
            "",
            &hosts(),
        )
        .into_iter()
        .map(|(_, why)| why)
        .collect();
        assert_eq!(why, ["gh pr checks"]);
    }

    #[test]
    fn one_reference_named_twice_is_watched_once() {
        assert_eq!(
            names(
                "gh pr view https://github.com/dpep/inkling/pull/7",
                "url:\thttps://github.com/dpep/inkling/pull/7\n"
            )
            .len(),
            1
        );
    }

    #[test]
    fn a_huge_output_is_not_read_to_the_end() {
        // A megabyte of somebody's test output must not be the cost of every
        // `git push`.
        let mut output = "x".repeat(OUTPUT_LIMIT + 10);
        output.push_str("https://github.com/dpep/inkling/pull/7");
        assert!(names("git push", &output).is_empty());
    }

    #[test]
    fn a_command_about_a_hundred_pull_requests_is_about_none_of_them() {
        // Measured: 64KiB of URLs registered 1,586 watches from one tool call,
        // and the poll that followed asked GitHub for all of them in one
        // 481KiB query. The cap is where that stops being possible.
        let output: String = (1..=100)
            .map(|n| format!("https://github.com/dpep/inkling/pull/{n}\n"))
            .collect();
        assert_eq!(names("gh pr view 1", &output).len(), MAX_REFERENCES);
    }
}
