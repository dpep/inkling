//! Naming a Slack message: from a permalink someone pasted, from the form
//! inkling prints, or from what a tool that just posted one handed back.
//!
//! The rule shared with the rest of `post-tool`: a target comes from what a
//! command was *about* — its arguments and its own structured result — never
//! from prose. A message's text is the session's own words or somebody else's,
//! and a permalink inside it points at a different message.

use crate::domain::model::Reference;
use serde_json::Value;

/// The tail every Slack permalink shares, whatever the workspace.
const ARCHIVES: &str = ".slack.com/archives/";

/// The Web API method a script posts with.
pub const POST_METHOD: &str = "chat.postMessage";

/// A Slack message, given as a permalink or as `slack:<channel>/<ts>`.
pub fn parse_reference(arg: &str) -> Option<Reference> {
    let arg = arg.trim().trim_start_matches('<').trim_end_matches('>');
    if let Some(rest) = arg.strip_prefix("slack:") {
        let (channel, ts) = rest.split_once('/')?;
        return (is_channel(channel) && is_ts(ts)).then(|| Reference::SlackMessage {
            channel: channel.to_string(),
            ts: ts.to_string(),
            thread: None,
        });
    }
    let (_, tail) = arg.split_once(ARCHIVES)?;
    permalink_tail(tail)
}

/// Enough of a Slack shape that reading it as anything else would hide a typo.
pub fn looks_like_slack(arg: &str) -> bool {
    arg.contains(ARCHIVES) || arg.starts_with("slack:")
}

/// `C0123ABC/p1786596050245879?thread_ts=…` — what follows `/archives/`.
fn permalink_tail(tail: &str) -> Option<Reference> {
    let (path, query) = tail.split_once('?').unwrap_or((tail, ""));
    let (channel, message) = path.trim_end_matches('/').split_once('/')?;
    let ts = ts_from_permalink(message)?;
    let thread = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("thread_ts="))
        .filter(|root| is_ts(root) && *root != ts)
        .map(str::to_string);
    is_channel(channel).then(|| Reference::SlackMessage {
        channel: channel.to_string(),
        ts,
        thread,
    })
}

/// `p1786596050245879` → `1786596050.245879`. A permalink drops the point;
/// the microseconds are always the last six digits.
fn ts_from_permalink(segment: &str) -> Option<String> {
    let digits = segment.strip_prefix('p')?;
    if digits.len() <= 6 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (seconds, micros) = digits.split_at(digits.len() - 6);
    Some(format!("{seconds}.{micros}"))
}

/// `C`, `D` or `G` and uppercase alphanumerics — a conversation, not a person.
fn is_channel(id: &str) -> bool {
    id.len() >= 9
        && id.starts_with(['C', 'D', 'G'])
        && id
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// A user id, which `slack_send_message` accepts as a channel to DM them.
fn is_user(id: &str) -> bool {
    id.len() >= 9
        && id.starts_with(['U', 'W'])
        && id
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

pub fn is_ts(ts: &str) -> bool {
    ts_key(ts).is_some()
}

/// A ts as something that orders. Compared as numbers, not strings or
/// floats: a float loses the microseconds, and strings only order while every
/// ts has ten digits of seconds.
pub fn ts_key(ts: &str) -> Option<(u64, u32)> {
    let (seconds, micros) = ts.split_once('.')?;
    if micros.len() != 6 {
        return None;
    }
    Some((seconds.parse().ok()?, micros.parse().ok()?))
}

/// Whether `ts` was posted after `than`. Unparseable is never later.
pub fn later(ts: &str, than: &str) -> bool {
    matches!((ts_key(ts), ts_key(than)), (Some(a), Some(b)) if a > b)
}

/// The message a `chat.postMessage` call printed it had posted.
///
/// The Web API's reply is a documented contract — `{"ok": true, "channel",
/// "ts", "message": {…}}` — so it is read as JSON or not at all. A command
/// that merely mentions the method, or a reply that says `ok: false`, posted
/// nothing.
pub fn posted_via_api(command: &str, stdout: &str) -> Option<Reference> {
    if !command.contains(POST_METHOD) {
        return None;
    }
    let reply: Value = serde_json::from_str(stdout.trim()).ok()?;
    if reply.get("ok")?.as_bool()? {
        let channel = reply.get("channel")?.as_str()?;
        let ts = reply.get("ts")?.as_str()?;
        let thread = reply
            .pointer("/message/thread_ts")
            .and_then(Value::as_str)
            .filter(|root| *root != ts)
            .map(str::to_string);
        return (is_channel(channel) && is_ts(ts)).then(|| Reference::SlackMessage {
            channel: channel.to_string(),
            ts: ts.to_string(),
            thread,
        });
    }
    None
}

/// The message a Slack MCP send tool just posted.
///
/// The channel is the one the call named; the ts can only come from the
/// result, so that is searched for a permalink into that channel. Two guards
/// keep prose out: a permalink the session itself wrote into the message is
/// not the message, and more than one candidate is no answer at all. A DM
/// sent to a user id lands in a `D` channel nobody named, so there any DM
/// permalink counts.
pub fn posted_via_tool(input: &Value, response: &Value) -> Option<Reference> {
    let channel = input.get("channel_id")?.as_str()?;
    let written = input
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let thread = input
        .get("thread_ts")
        .and_then(Value::as_str)
        .filter(|root| is_ts(root));

    let mut found: Vec<Reference> = Vec::new();
    for text in strings(response) {
        for tail in tails(&text) {
            if written.contains(tail) {
                continue;
            }
            let Some(Reference::SlackMessage {
                channel: posted,
                ts,
                thread: linked,
            }) = permalink_tail(tail)
            else {
                continue;
            };
            let ours = posted == channel || (is_user(channel) && posted.starts_with('D'));
            let reference = Reference::SlackMessage {
                channel: posted,
                thread: thread.map(str::to_string).or(linked).filter(|r| *r != ts),
                ts,
            };
            if ours && !found.contains(&reference) {
                found.push(reference);
            }
        }
    }
    match found.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    }
}

/// Every permalink tail in `text`, cut where a URL can't continue.
fn tails(text: &str) -> Vec<&str> {
    text.match_indices(ARCHIVES)
        .map(|(at, _)| {
            let tail = &text[at + ARCHIVES.len()..];
            let end = tail
                .find(|c: char| c.is_whitespace() || "\"'<>()[]|,`".contains(c))
                .unwrap_or(tail.len());
            &tail[..end]
        })
        .collect()
}

/// Every string in a tool result, however it is nested — MCP results arrive
/// as content blocks whose text is often JSON again, with its slashes escaped.
fn strings(value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    collect(value, &mut out, 0);
    out
}

fn collect(value: &Value, out: &mut Vec<String>, depth: usize) {
    if depth > 8 {
        return;
    }
    match value {
        Value::String(text) => match serde_json::from_str::<Value>(text) {
            Ok(inner @ (Value::Object(_) | Value::Array(_))) => collect(&inner, out, depth + 1),
            _ => out.push(text.replace("\\/", "/")),
        },
        Value::Array(items) => items.iter().for_each(|v| collect(v, out, depth + 1)),
        Value::Object(fields) => fields.values().for_each(|v| collect(v, out, depth + 1)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message(channel: &str, ts: &str, thread: Option<&str>) -> Reference {
        Reference::SlackMessage {
            channel: channel.into(),
            ts: ts.into(),
            thread: thread.map(Into::into),
        }
    }

    #[test]
    fn a_permalink_names_its_message() {
        assert_eq!(
            parse_reference("https://acme.slack.com/archives/C0123ABCD/p1786596050245879"),
            Some(message("C0123ABCD", "1786596050.245879", None))
        );
    }

    #[test]
    fn a_reply_s_permalink_carries_the_root_its_replies_hang_from() {
        assert_eq!(
            parse_reference(
                "https://acme.slack.com/archives/C0123ABCD/p1786596050245879?thread_ts=1786596000.123456&cid=C0123ABCD"
            ),
            Some(message(
                "C0123ABCD",
                "1786596050.245879",
                Some("1786596000.123456")
            ))
        );
    }

    #[test]
    fn the_form_inkling_prints_is_accepted_back() {
        assert_eq!(
            parse_reference("slack:D1MG8KWNR/1771013011.519909"),
            Some(message("D1MG8KWNR", "1771013011.519909", None))
        );
    }

    #[test]
    fn something_slack_shaped_but_broken_is_not_a_message() {
        for arg in [
            "https://acme.slack.com/archives/C0123ABCD",
            "https://acme.slack.com/archives/C0123ABCD/p123",
            "https://acme.slack.com/archives/general/p1786596050245879",
            "slack:C0123ABCD/1786596050",
        ] {
            assert_eq!(parse_reference(arg), None, "{arg}");
            assert!(looks_like_slack(arg), "{arg}");
        }
    }

    #[test]
    fn timestamps_order_as_numbers() {
        assert!(later("1786596050.245880", "1786596050.245879"));
        assert!(
            later("10000000000.000000", "9999999999.999999"),
            "not as strings"
        );
        assert!(!later("nonsense", "1786596050.245879"));
    }

    // The documented Web API reply, as `curl` prints it.
    const POSTED: &str = r#"{"ok":true,"channel":"C0123ABCD","ts":"1786596050.245879","message":{"type":"message","text":"see https://acme.slack.com/archives/C0999ZZZZ/p1700000000000001","ts":"1786596050.245879"}}"#;

    #[test]
    fn a_curl_to_chat_post_message_names_what_it_posted() {
        let command = "curl -s -X POST https://slack.com/api/chat.postMessage -d @body.json";
        assert_eq!(
            posted_via_api(command, POSTED),
            Some(message("C0123ABCD", "1786596050.245879", None))
        );
    }

    #[test]
    fn a_reply_posted_through_the_api_knows_its_thread() {
        let reply = r#"{"ok":true,"channel":"C0123ABCD","ts":"1786596050.245879","message":{"ts":"1786596050.245879","thread_ts":"1786596000.123456"}}"#;
        assert_eq!(
            posted_via_api("curl https://slack.com/api/chat.postMessage", reply),
            Some(message(
                "C0123ABCD",
                "1786596050.245879",
                Some("1786596000.123456")
            ))
        );
    }

    #[test]
    fn a_refused_post_or_another_method_posted_nothing() {
        let refused = r#"{"ok":false,"error":"channel_not_found"}"#;
        assert_eq!(
            posted_via_api("curl https://slack.com/api/chat.postMessage", refused),
            None
        );
        assert_eq!(
            posted_via_api("curl https://slack.com/api/chat.update", POSTED),
            None
        );
        assert_eq!(
            posted_via_api("curl https://slack.com/api/chat.postMessage", "not json"),
            None
        );
    }

    fn sent(channel: &str, text: &str) -> Value {
        json!({"channel_id": channel, "message": text})
    }

    #[test]
    fn a_sent_message_is_found_by_its_permalink_into_the_channel_named() {
        // Content blocks whose text is JSON again, with escaped slashes: the
        // shape the connector's other tools answer in.
        let response = json!([{"type": "text", "text": r#"{"message_link":"https:\/\/acme.slack.com\/archives\/C0123ABCD\/p1786596050245879"}"#}]);
        assert_eq!(
            posted_via_tool(&sent("C0123ABCD", "hello"), &response),
            Some(message("C0123ABCD", "1786596050.245879", None))
        );
    }

    #[test]
    fn a_permalink_the_session_wrote_is_not_the_message_it_sent() {
        let written = "see https://acme.slack.com/archives/C0123ABCD/p1700000000000001";
        let response = json!({"text": format!(
            "Sent. {written} Link: https://acme.slack.com/archives/C0123ABCD/p1786596050245879"
        )});
        assert_eq!(
            posted_via_tool(&sent("C0123ABCD", written), &response),
            Some(message("C0123ABCD", "1786596050.245879", None))
        );
    }

    #[test]
    fn a_permalink_into_another_channel_or_two_candidates_is_no_answer() {
        let elsewhere = json!("https://acme.slack.com/archives/C0999ZZZZ/p1786596050245879");
        assert_eq!(posted_via_tool(&sent("C0123ABCD", "hi"), &elsewhere), None);

        let two = json!(
            "https://acme.slack.com/archives/C0123ABCD/p1786596050245879 https://acme.slack.com/archives/C0123ABCD/p1786596050245880"
        );
        assert_eq!(posted_via_tool(&sent("C0123ABCD", "hi"), &two), None);
    }

    #[test]
    fn a_reply_takes_its_thread_from_the_call() {
        let input =
            json!({"channel_id": "C0123ABCD", "message": "hi", "thread_ts": "1786596000.123456"});
        let response = json!("https://acme.slack.com/archives/C0123ABCD/p1786596050245879");
        assert_eq!(
            posted_via_tool(&input, &response),
            Some(message(
                "C0123ABCD",
                "1786596050.245879",
                Some("1786596000.123456")
            ))
        );
    }

    #[test]
    fn a_dm_to_a_user_lands_in_a_dm_channel() {
        let response = json!("https://acme.slack.com/archives/D1MG8KWNR/p1786596050245879");
        assert_eq!(
            posted_via_tool(&sent("U0E48AHQA", "hi"), &response),
            Some(message("D1MG8KWNR", "1786596050.245879", None))
        );
    }
}
