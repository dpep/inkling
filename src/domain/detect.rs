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
    /// A bare number. Ambiguous until matched against tracked records —
    /// guessing a repository would react to the wrong pull request.
    Number(u64),
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

    let digits = trimmed.strip_prefix('#').unwrap_or(trimmed);
    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        return match digits.parse() {
            Ok(number) => Ok(Target::Number(number)),
            Err(_) => Err(format!("{trimmed}: pull request number is out of range")),
        };
    }

    if looks_like_a_pull_request(trimmed) {
        return Err(format!(
            "{trimmed}: looks like a pull request but isn't one — expected \
             https://<host>/<owner>/<repo>/pull/<number>"
        ));
    }
    Ok(Target::Channel(trimmed.to_string()))
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

/// Enough of a pull-request shape that treating it as a channel would hide a
/// typo rather than do what was asked.
fn looks_like_a_pull_request(arg: &str) -> bool {
    let lower = arg.to_lowercase();
    lower.contains("/pull/") || lower.contains("github.")
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

    #[test]
    fn a_bare_number_stays_ambiguous_for_the_caller_to_resolve() {
        assert_eq!(target("365304").unwrap(), Target::Number(365304));
        assert_eq!(target("#2").unwrap(), Target::Number(2));
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
