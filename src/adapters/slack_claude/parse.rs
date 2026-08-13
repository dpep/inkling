//! Reading what came back from the bridge.
//!
//! The model is asked for strict JSON, but it's still a model: it wraps
//! output in code fences, prepends a sentence, or truncates. Parsing is
//! written to survive all three and to fail loudly rather than silently
//! returning nothing — an empty result is indistinguishable from "no
//! messages", which would quietly stall discovery forever.

use crate::domain::model::{Message, MessageId, ReactionName};
use chrono::{DateTime, Utc};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct SearchHit {
    pub channel_id: String,
    pub ts: String,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub permalink: String,
    #[serde(default)]
    pub text: String,
}

impl SearchHit {
    pub fn into_message(self) -> Message {
        let posted_at = slack_ts_to_time(&self.ts).unwrap_or_else(Utc::now);
        Message {
            id: MessageId::new(&self.channel_id, &self.ts),
            text: self.text,
            author: self.user_id,
            posted_at,
            permalink: self.permalink,
            reactions: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    messages: Vec<SearchHit>,
}

#[derive(Debug, Deserialize)]
struct ReactionsResponse {
    messages: Vec<ReactionHit>,
}

#[derive(Debug, Deserialize)]
struct ReactionHit {
    ts: String,
    #[serde(default)]
    reactions: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Reaction {
    pub ts: String,
    pub names: Vec<ReactionName>,
}

pub fn parse_search(raw: &str) -> Result<Vec<SearchHit>, String> {
    let json = extract_json(raw)?;
    let response: SearchResponse =
        serde_json::from_str(&json).map_err(|e| format!("search response: {e}"))?;
    Ok(response.messages)
}

pub fn parse_reactions(raw: &str) -> Result<Vec<Reaction>, String> {
    let json = extract_json(raw)?;
    let response: ReactionsResponse =
        serde_json::from_str(&json).map_err(|e| format!("reactions response: {e}"))?;
    Ok(response
        .messages
        .into_iter()
        .map(|hit| Reaction {
            ts: hit.ts,
            names: hit.reactions.iter().map(ReactionName::new).collect(),
        })
        .collect())
}

/// Pull the JSON object out of whatever the model actually said. Handles a
/// bare object, a ```json fence, and a leading sentence; brace-matching
/// rather than a regex so nested objects survive.
fn extract_json(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("bridge returned nothing".into());
    }

    let start = trimmed
        .find('{')
        .ok_or_else(|| format!("no JSON object in bridge output: {}", preview(trimmed)))?;

    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in trimmed[start..].char_indices() {
        if in_string {
            match ch {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(trimmed[start..start + offset + 1].to_string());
                }
            }
            _ => {}
        }
    }
    // Unbalanced braces mean the reply was cut off. Reporting that beats
    // returning an empty list, which would read as "nothing to do".
    Err(format!(
        "truncated JSON in bridge output: {}",
        preview(trimmed)
    ))
}

fn preview(s: &str) -> String {
    let cleaned = s.replace('\n', " ");
    match cleaned.char_indices().nth(120) {
        Some((cut, _)) => format!("{}…", &cleaned[..cut]),
        None => cleaned,
    }
}

/// Slack timestamps are `seconds.microseconds` since the epoch.
fn slack_ts_to_time(ts: &str) -> Option<DateTime<Utc>> {
    let (seconds, fraction) = ts.split_once('.').unwrap_or((ts, "0"));
    let seconds: i64 = seconds.parse().ok()?;
    let micros: u32 = format!("{fraction:0<6}")[..6].parse().ok()?;
    DateTime::from_timestamp(seconds, micros * 1_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH: &str = r#"{"messages":[
        {"channel_id":"C0BPR3AEAG3","ts":"1786596050.245879","user_id":"U0E48AHQA",
         "permalink":"https://dpepper.slack.com/archives/C0BPR3AEAG3/p1786596050245879",
         "text":"test fixture — merged PR https://github.com/dpep/gqls/pull/1"}
    ]}"#;

    #[test]
    fn a_clean_search_response_parses() {
        let hits = parse_search(SEARCH).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].channel_id, "C0BPR3AEAG3");
        assert_eq!(hits[0].ts, "1786596050.245879");
    }

    #[test]
    fn a_code_fence_is_stripped() {
        // Haiku wraps JSON in fences; observed, not hypothetical.
        let raw = format!("```json\n{SEARCH}\n```");
        assert_eq!(parse_search(&raw).unwrap().len(), 1);
    }

    #[test]
    fn a_leading_sentence_is_ignored() {
        let raw = format!("Here are the results:\n\n{SEARCH}");
        assert_eq!(parse_search(&raw).unwrap().len(), 1);
    }

    #[test]
    fn an_empty_result_set_is_not_an_error() {
        assert!(parse_search(r#"{"messages": []}"#).unwrap().is_empty());
    }

    #[test]
    fn truncated_output_is_an_error_not_an_empty_list() {
        // The dangerous failure: silently reading as "nothing to do" would
        // stall discovery while looking healthy.
        let raw = r#"{"messages":[{"channel_id":"C1","ts":"1"#;
        let error = parse_search(raw).unwrap_err();
        assert!(error.contains("truncated"), "{error}");
    }

    #[test]
    fn prose_instead_of_json_is_an_error() {
        let error = parse_search("I could not find any messages.").unwrap_err();
        assert!(error.contains("no JSON object"), "{error}");
    }

    #[test]
    fn empty_output_is_an_error() {
        assert!(parse_search("").is_err());
        assert!(parse_search("   \n ").is_err());
    }

    #[test]
    fn a_wrong_shape_is_an_error() {
        assert!(parse_search(r#"{"results": "some markdown"}"#).is_err());
    }

    #[test]
    fn braces_inside_message_text_do_not_end_the_object() {
        let raw = r#"{"messages":[{"channel_id":"C1","ts":"1.0","user_id":"U1",
            "permalink":"p","text":"fix: use {braces} and a \" quote"}]}"#;
        let hits = parse_search(raw).unwrap();
        assert!(hits[0].text.contains("{braces}"));
    }

    #[test]
    fn reactions_parse_and_normalize() {
        let raw = r#"{"messages":[
            {"ts":"1786596050.245879","reactions":[":Merged:","white_check_mark"]},
            {"ts":"1786596055.150629","reactions":[]}
        ]}"#;
        let parsed = parse_reactions(raw).unwrap();
        assert_eq!(
            parsed[0].names,
            vec![
                ReactionName::new("merged"),
                ReactionName::new("white_check_mark")
            ]
        );
        assert!(parsed[1].names.is_empty());
    }

    #[test]
    fn a_slack_timestamp_becomes_a_time() {
        let hit = SearchHit {
            channel_id: "C1".into(),
            ts: "1786596050.245879".into(),
            user_id: "U1".into(),
            permalink: String::new(),
            text: String::new(),
        };
        let message = hit.into_message();
        // The real ts of a fixture message posted to #inkling.
        assert_eq!(
            message
                .posted_at
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "2026-08-13T04:40:50Z"
        );
    }
}
