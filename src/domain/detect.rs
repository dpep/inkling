//! Finding the external things a Slack message points at.
//!
//! The hard half — pulling URLs out of prose and parsing them into parts — is
//! `iriq`'s. What is left is a structural match on the path shape, which is
//! why repo roots, `/issues/7`, and `/commit/<sha>` need no rejection rules:
//! they simply don't match.

use crate::domain::model::Reference;
use iriq::Extractor;

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

    #[test]
    fn finds_nothing_in_text_without_links() {
        assert!(detected("just a normal message about a pull request").is_empty());
        assert!(detected("").is_empty());
    }
}
