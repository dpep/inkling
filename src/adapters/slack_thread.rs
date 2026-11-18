//! Slack messages as a watch source: one thread read per watched message,
//! through the claude → Slack MCP bridge.
//!
//! Each read is a `claude -p` call — about 2¢ and anywhere from a few seconds
//! to a minute — which is why the schedule puts a floor under how often a
//! message is read (`watches_json::SLACK_READ_FLOOR`) and why only callers
//! with time to spare hand the poller this provider.
//!
//! Names come with the read: the thread tool prints each author's name beside
//! their id, so naming who replied costs no profile lookup.
//!
//! The channel after the message is read in the same call. In a DM every
//! later message is a reply, threaded or not: there the next message is the
//! answer. Elsewhere a later message is a reply when it @mentions the reader.
//! Either way a reply counts for one watched message: the latest before it.

use crate::adapters::slack_claude::{ActivityError, Runner, SlackClaude};
use crate::domain::model::Reference;
use crate::domain::slack::{is_dm, owner};
use crate::ports::{
    ChannelStatus, MessageLookup, MessageProvider, MessageStatus, ProviderError, RefError,
};
use std::cell::{Cell, OnceCell};
use std::collections::HashSet;
use std::time::{Duration, Instant};

pub struct SlackThreads<R: Runner> {
    slack: SlackClaude<R>,
    /// Asked only once a message is actually read: finding out who the
    /// credentials belong to may itself be a bridge call.
    identify: Box<dyn Fn() -> Option<String>>,
    me: OnceCell<String>,
    /// When finding out last failed. A failure isn't kept for the life of a
    /// server, but isn't retried on every read either: each try is paid.
    failed: Cell<Option<Instant>>,
    retry: Duration,
}

impl<R: Runner> SlackThreads<R> {
    pub fn new(slack: SlackClaude<R>, identify: impl Fn() -> Option<String> + 'static) -> Self {
        Self {
            slack,
            identify: Box::new(identify),
            me: OnceCell::new(),
            failed: Cell::new(None),
            retry: crate::adapters::watches_json::SLACK_READ_FLOOR
                .to_std()
                .unwrap_or_default(),
        }
    }

    /// The reader's Slack user id, which is how a channel message that
    /// @mentions them is recognized as a reply.
    fn me(&self) -> Option<String> {
        if let Some(me) = self.me.get() {
            return Some(me.clone());
        }
        if self
            .failed
            .get()
            .is_some_and(|at| at.elapsed() < self.retry)
        {
            return None;
        }
        match (self.identify)() {
            Some(me) => Some(self.me.get_or_init(|| me).clone()),
            None => {
                self.failed.set(Some(Instant::now()));
                None
            }
        }
    }

    fn read_one(
        &self,
        reference: &Reference,
        watched: &[Reference],
    ) -> Result<MessageStatus, RefError> {
        let Reference::SlackMessage {
            channel,
            ts,
            thread,
        } = reference
        else {
            return Err(RefError::Other("not a slack message".into()));
        };
        let root = thread.as_deref().unwrap_or(ts);
        let read = self
            .slack
            .activity(channel, root, ts)
            .map_err(|error| match error {
                ActivityError::Gone(_) => RefError::NotFound,
                ActivityError::Failed(why) => RefError::Other(why),
            })?;

        let mut status = MessageStatus::default();
        if let Some(watched) = read.thread.iter().find(|entry| entry.ts == *ts) {
            status.reactions = watched.reactions.clone();
        }
        let dm = is_dm(channel);
        let me = if dm { None } else { self.me() };
        // A thread reply also sent to the channel is in both: it is a thread's.
        let in_thread: HashSet<String> = read.thread.iter().map(|e| e.ts.clone()).collect();
        let answers = read
            .channel
            .into_iter()
            .flatten()
            .filter(|entry| !in_thread.contains(&entry.ts))
            .filter(|entry| dm || me.as_ref().is_some_and(|me| entry.mentions.contains(me)))
            .map(|entry| (entry, None));
        let thread = read.thread.into_iter().map(|entry| (entry, Some(root)));
        let watched = || watched.iter().chain(std::iter::once(reference));
        for (entry, thread) in thread.chain(answers) {
            if !entry.user_id.is_empty() && !entry.user_name.is_empty() {
                status
                    .names
                    .insert(entry.user_id.clone(), entry.user_name.clone());
            }
            // Only the ones after it (what came before was already there),
            // and not one a later watched message heard instead.
            if owner(channel, &entry.ts, thread, watched()) == Some(ts.as_str()) {
                status.replies.insert(entry.ts, entry.user_id);
            }
        }
        Ok(status)
    }
}

impl<R: Runner> MessageProvider for SlackThreads<R> {
    /// One call per message: the thread tool reads one thread, and a channel
    /// read lists top-level messages only.
    fn read(
        &self,
        messages: &[Reference],
        watched: &[Reference],
    ) -> Result<Vec<MessageLookup>, ProviderError> {
        Ok(messages
            .iter()
            .map(|reference| (reference.clone(), self.read_one(reference, watched)))
            .collect())
    }

    /// One `read_channel`, no thread reads: a reply in a thread is that
    /// thread's news, and one also sent to the channel is top-level anyway.
    fn channel(&self, channel: &str, oldest: Option<&str>) -> Result<ChannelStatus, RefError> {
        let read = self
            .slack
            .channel_messages(channel, oldest)
            .map_err(|error| match error {
                ActivityError::Gone(_) => RefError::NotFound,
                ActivityError::Failed(why) => RefError::Other(why),
            })?;
        let mut status = ChannelStatus {
            more: read.more,
            ..ChannelStatus::default()
        };
        for entry in read.channel {
            if !entry.user_id.is_empty() && !entry.user_name.is_empty() {
                status
                    .names
                    .insert(entry.user_id.clone(), entry.user_name.clone());
            }
            status.arrived.insert(entry.ts, entry.user_id);
        }
        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::slack_claude::{
        REAL_CHANNEL_BASELINE, REAL_CHANNEL_READ, REAL_DM_ACTIVITY, REAL_EMPTY_CHANNEL,
        REAL_THREAD, RunnerError,
    };
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;
    use std::rc::Rc;

    /// Replays canned bridge replies and keeps the prompts it was given.
    struct Replay {
        replies: RefCell<Vec<String>>,
        prompts: Rc<RefCell<Vec<String>>>,
    }

    impl Runner for Replay {
        fn run(&self, prompt: &str, _: &[&str], _: usize) -> Result<String, RunnerError> {
            self.prompts.borrow_mut().push(prompt.to_string());
            Ok(self.replies.borrow_mut().pop().unwrap_or_default())
        }
    }

    fn threads(replies: &[&str]) -> (SlackThreads<Replay>, Rc<RefCell<Vec<String>>>) {
        let prompts = Rc::new(RefCell::new(Vec::new()));
        let runner = Replay {
            replies: RefCell::new(replies.iter().rev().map(|s| s.to_string()).collect()),
            prompts: prompts.clone(),
        };
        let slack = SlackClaude::new(runner).without_delay();
        (
            SlackThreads::new(slack, || Some("U00000009".into())),
            prompts,
        )
    }

    fn message(channel: &str, ts: &str, thread: Option<&str>) -> Reference {
        Reference::SlackMessage {
            channel: channel.into(),
            ts: ts.into(),
            thread: thread.map(Into::into),
        }
    }

    #[test]
    fn in_a_dm_a_later_message_is_a_reply_threaded_or_not() {
        // The real case: a :dart: on the watched message, then a top-level
        // message in the DM rather than a thread reply.
        let (slack, prompts) = threads(&[REAL_DM_ACTIVITY]);
        let watched = message("D1MG8KWNR", "1794976101.418449", None);
        let status = slack.read(&[watched], &[]).unwrap()[0].1.clone().unwrap();

        let prompt = &prompts.borrow()[0];
        assert!(prompt.contains("slack_read_channel"), "{prompt}");
        assert!(
            prompt.contains(r#"oldest: "1794976101.418449""#),
            "{prompt}"
        );
        assert_eq!(status.reactions, BTreeMap::from([("dart".into(), 1)]));
        assert_eq!(
            status.replies,
            BTreeMap::from([("1794976230.118559".into(), "U0E48AHQA".into())])
        );
        assert_eq!(
            status.names.get("U0E48AHQA").map(String::as_str),
            Some("dpepper")
        );
    }

    #[test]
    fn a_dm_read_that_skipped_the_channel_is_retried() {
        // An empty list would read as a quiet DM; a missing one is a model
        // that made one call of the two.
        let (slack, prompts) = threads(&[REAL_THREAD, REAL_DM_ACTIVITY]);
        let read = slack.read(&[message("D1MG8KWNR", "1794976101.418449", None)], &[]);
        assert_eq!(read.unwrap()[0].1.clone().unwrap().replies.len(), 1);
        assert_eq!(prompts.borrow().len(), 2);
    }

    /// Invented: this workspace has no channel message that @mentions anyone
    /// to record, so the shape is the prompt's, not a transcript's.
    const CHANNEL: &str = r#"{"messages":[
        {"ts":"1786596000.123456","user_id":"U00000009","user_name":"dpepper","reactions":{}}
    ],"channel":[
        {"ts":"1786596100.000001","user_id":"U00000001","user_name":"alice","mentions":["U00000009"]},
        {"ts":"1786596200.000001","user_id":"U00000002","user_name":"bob","mentions":[]},
        {"ts":"1786596300.000001","user_id":"U00000003","user_name":"carol","mentions":["U00000007"]}
    ]}"#;

    #[test]
    fn in_a_channel_a_later_message_is_a_reply_only_when_it_mentions_me() {
        let (slack, prompts) = threads(&[CHANNEL]);
        let status = slack
            .read(&[message("C0123ABCD", "1786596000.123456", None)], &[])
            .unwrap()[0]
            .1
            .clone()
            .unwrap();

        assert!(prompts.borrow()[0].contains("slack_read_channel"));
        assert_eq!(
            status.replies,
            BTreeMap::from([("1786596100.000001".into(), "U00000001".into())])
        );
    }

    const THREAD: &str = r#"{"messages":[
        {"ts":"1786596000.123456","user_id":"U00000009","user_name":"dpepper","reactions":{}},
        {"ts":"1786596050.245879","user_id":"U00000001","user_name":"alice","reactions":{"eyes":1}},
        {"ts":"1786596060.000001","user_id":"U00000002","user_name":"bob","reactions":{}}
    ],"channel":[]}"#;

    #[test]
    fn a_reply_is_read_from_its_root_and_counts_only_what_came_after_it() {
        let (slack, prompts) = threads(&[THREAD]);
        let reply = message("C0123ABCD", "1786596050.245879", Some("1786596000.123456"));
        let status = slack.read(&[reply], &[]).unwrap()[0].1.clone().unwrap();

        assert!(prompts.borrow()[0].contains(r#"message_ts: "1786596000.123456""#));
        assert!(
            prompts.borrow()[0].contains(r#"oldest: "1786596050.245878""#),
            "a reply past the first page of its thread is still read"
        );
        assert_eq!(status.reactions, BTreeMap::from([("eyes".into(), 1)]));
        assert_eq!(
            status.replies,
            BTreeMap::from([("1786596060.000001".into(), "U00000002".into())])
        );
    }

    /// Invented, in the shape of the real reads: a DM thread with replies
    /// in it, then top-level messages after it.
    const DM: &str = r#"{"messages":[
        {"ts":"1786596000.123456","user_id":"U00000009","user_name":"dpepper","reactions":{}},
        {"ts":"1786596050.245879","user_id":"U00000001","user_name":"alice","reactions":{}},
        {"ts":"1786596060.000001","user_id":"U00000002","user_name":"bob","reactions":{}}
    ],"channel":[
        {"ts":"1786596100.000001","user_id":"U00000009","user_name":"dpepper","mentions":[]},
        {"ts":"1786596200.000001","user_id":"U00000001","user_name":"alice","mentions":[]}
    ]}"#;

    fn replies(raw: &str, of: &Reference, watched: &[Reference]) -> Vec<String> {
        let (slack, _) = threads(&[raw]);
        let status = slack.read(std::slice::from_ref(of), watched).unwrap()[0]
            .1
            .clone()
            .unwrap();
        status.replies.into_keys().collect()
    }

    #[test]
    fn a_dm_answer_is_news_for_the_latest_watched_message_only() {
        // Seen live: one answer pushed once per earlier watched message.
        let first = message("D0000000A", "1786596000.123456", None);
        let second = message("D0000000A", "1786596100.000001", None);
        let watched = [first.clone(), second.clone()];
        // The second is itself the next message after the first.
        assert_eq!(
            replies(DM, &first, &watched),
            [
                "1786596050.245879",
                "1786596060.000001",
                "1786596100.000001"
            ],
        );
        let after = r#"{"messages":[
            {"ts":"1786596100.000001","user_id":"U00000009","user_name":"dpepper","reactions":{}}
        ],"channel":[
            {"ts":"1786596200.000001","user_id":"U00000001","user_name":"alice","mentions":[]}
        ]}"#;
        assert_eq!(replies(after, &second, &watched), ["1786596200.000001"]);
    }

    #[test]
    fn a_thread_reply_is_news_for_the_latest_watched_message_in_the_thread() {
        let root = message("C0123ABCD", "1786596000.123456", None);
        let reply = message("C0123ABCD", "1786596050.245879", Some("1786596000.123456"));
        let watched = [root.clone(), reply.clone()];
        assert_eq!(replies(THREAD, &root, &watched), ["1786596050.245879"]);
        assert_eq!(replies(THREAD, &reply, &watched), ["1786596060.000001"]);
    }

    #[test]
    fn a_message_watched_alone_hears_every_reply() {
        // Including when the poller didn't list it among the watched.
        let root = message("D0000000A", "1786596000.123456", None);
        let all = [
            "1786596050.245879",
            "1786596060.000001",
            "1786596100.000001",
            "1786596200.000001",
        ];
        assert_eq!(replies(DM, &root, std::slice::from_ref(&root)), all);
        assert_eq!(replies(DM, &root, &[]), all);
    }

    #[test]
    fn a_thread_is_read_one_page_a_model_can_write_back() {
        let (slack, prompts) = threads(&[THREAD]);
        let _ = slack.read(&[message("C0123ABCD", "1786596000.123456", None)], &[]);
        let prompt = &prompts.borrow()[0];
        let thread = &prompt[..prompt.find("Then call").unwrap()];
        assert!(thread.contains("limit: 100\n"), "{thread}");
    }

    #[test]
    fn the_prompt_never_asks_for_what_a_message_says() {
        let (slack, prompts) = threads(&[THREAD]);
        let _ = slack.read(&[message("C0123ABCD", "1786596000.123456", None)], &[]);
        let prompt = &prompts.borrow()[0];
        assert!(prompt.contains("Do not include message text"), "{prompt}");
        assert!(!prompt.contains("\"text\""), "{prompt}");
    }

    #[test]
    fn a_read_that_parsed_without_the_watched_message_is_not_found_and_not_retried() {
        // A deleted reply's thread still reads, with no error code to match.
        let dropped = r#"{"messages":[{"ts":"1786596060.000001","user_id":"U00000002","reactions":{}}],"channel":[]}"#;
        let (slack, prompts) = threads(&[dropped, THREAD]);
        let read = slack.read(&[message("C0123ABCD", "1786596000.123456", None)], &[]);
        assert_eq!(read.unwrap()[0].1, Err(RefError::NotFound));
        assert_eq!(prompts.borrow().len(), 1);
    }

    #[test]
    fn a_read_with_no_thread_rows_is_retried_not_gone() {
        // A model without the Slack tools may echo the template, whose rows
        // are dropped as unreadable: nothing was read, so nothing is missing.
        let template = r#"I can't reach Slack. The output would look like: {"messages": [{"ts": "...", "user_id": "...", "user_name": "...", "reactions": {"name": 1}}], "channel": [{"ts": "...", "user_id": "...", "user_name": "...", "mentions": ["U..."]}]}"#;
        let empty = r#"{"messages":[],"channel":[]}"#;
        let (slack, prompts) = threads(&[template, empty, THREAD]);
        let read = slack.read(&[message("C0123ABCD", "1786596000.123456", None)], &[]);
        assert!(read.unwrap()[0].1.is_ok());
        assert_eq!(prompts.borrow().len(), 3);
    }

    #[test]
    fn a_thread_slack_says_is_gone_is_not_found_and_not_retried() {
        let (slack, prompts) = threads(&["execution_failed: thread_not_found", THREAD]);
        let read = slack.read(&[message("C0123ABCD", "1786596000.123456", None)], &[]);
        assert_eq!(read.unwrap()[0].1, Err(RefError::NotFound));
        assert_eq!(prompts.borrow().len(), 1);
    }

    #[test]
    fn an_error_code_inside_a_good_read_is_not_an_error() {
        // A display name that happens to spell one would otherwise end the
        // watch, now that gone finishes it.
        let named = THREAD.replace("\"bob\"", "\"message_not_found\"");
        let (slack, _) = threads(&[&named]);
        let read = slack.read(&[message("C0123ABCD", "1786596000.123456", None)], &[]);
        assert!(read.unwrap()[0].1.is_ok());

        let channel = REAL_CHANNEL_READ.replace("\"dpepper\"", "\"channel_not_found\"");
        let (slack, _) = threads(&[&channel]);
        assert!(
            slack
                .channel("D1MG8KWNR", Some("1771013015.064589"))
                .is_ok()
        );
    }

    #[test]
    fn a_channel_read_is_one_read_channel_from_where_the_last_one_ended() {
        let (slack, prompts) = threads(&[REAL_CHANNEL_READ]);
        let status = slack
            .channel("D1MG8KWNR", Some("1771013015.064589"))
            .unwrap();

        let prompt = &prompts.borrow()[0];
        assert!(prompt.contains("slack_read_channel"), "{prompt}");
        assert!(!prompt.contains("slack_read_thread"), "{prompt}");
        assert!(
            prompt.contains(r#"oldest: "1771013015.064589""#),
            "{prompt}"
        );
        assert!(prompt.contains("limit: 100"), "{prompt}");
        assert!(prompt.contains("Do not include message text"), "{prompt}");
        assert_eq!(
            status.arrived,
            BTreeMap::from([
                ("1794976101.418449".into(), "U0E48AHQA".into()),
                ("1794976230.118559".into(), "U0E48AHQA".into()),
            ])
        );
        assert!(!status.more);
        assert_eq!(
            status.names.get("U0E48AHQA").map(String::as_str),
            Some("dpepper")
        );
    }

    #[test]
    fn a_channel_baseline_asks_for_the_newest_alone() {
        let (slack, prompts) = threads(&[REAL_CHANNEL_BASELINE]);
        let status = slack.channel("D1MG8KWNR", None).unwrap();
        let prompt = &prompts.borrow()[0];
        assert!(!prompt.contains("oldest"), "{prompt}");
        assert!(prompt.contains("limit: 1\n"), "{prompt}");
        assert_eq!(status.arrived.len(), 1);
    }

    #[test]
    fn an_empty_channel_is_an_answer_and_a_skipped_call_is_retried() {
        let (slack, prompts) = threads(&[r#"{"messages": []}"#, REAL_EMPTY_CHANNEL]);
        let status = slack.channel("C0E48AHSN", Some("0")).unwrap();
        assert!(status.arrived.is_empty());
        assert_eq!(prompts.borrow().len(), 2);
    }

    #[test]
    fn a_full_page_is_more_whether_or_not_the_model_says_so() {
        let page: Vec<String> = (0..100)
            .map(|n| {
                format!(r#"{{"ts":"1794976{n:03}.000001","user_id":"U00000001","user_name":"a"}}"#)
            })
            .collect();
        let raw = format!(r#"{{"channel":[{}],"more":false}}"#, page.join(","));
        let (slack, _) = threads(&[&raw]);
        assert!(slack.channel("C0E48AHSN", Some("1.000000")).unwrap().more);
    }

    #[test]
    fn a_channel_slack_says_is_gone_is_not_found() {
        let (slack, _) = threads(&["channel_not_found"]);
        assert_eq!(
            slack.channel("C0E48AHSN", Some("0")),
            Err(RefError::NotFound)
        );
    }

    #[test]
    fn who_i_am_is_asked_once_and_only_when_wanted() {
        let asked = Rc::new(Cell::new(0));
        let counted = asked.clone();
        let runner = Replay {
            replies: RefCell::new(Vec::new()),
            prompts: Rc::new(RefCell::new(Vec::new())),
        };
        let slack = SlackThreads::new(SlackClaude::new(runner), move || {
            counted.set(counted.get() + 1);
            Some("U00000009".into())
        });
        assert_eq!(asked.get(), 0);
        assert_eq!(slack.me().as_deref(), Some("U00000009"));
        assert_eq!(slack.me().as_deref(), Some("U00000009"));
        assert_eq!(asked.get(), 1);
    }

    #[test]
    fn not_finding_out_who_i_am_is_asked_again_later_not_never() {
        // A server lives for days; one failed whoami must not blind the
        // mention rule for all of them.
        let answers = Rc::new(RefCell::new(vec![Some("U00000009"), None]));
        let left = answers.clone();
        let runner = Replay {
            replies: RefCell::new(Vec::new()),
            prompts: Rc::new(RefCell::new(Vec::new())),
        };
        let slack = SlackThreads::new(SlackClaude::new(runner), move || {
            left.borrow_mut().pop().flatten().map(Into::into)
        });
        assert_eq!(slack.me(), None);
        assert_eq!(slack.me(), None, "not asked again at once");
        assert_eq!(answers.borrow().len(), 1);

        let slack = SlackThreads {
            retry: std::time::Duration::ZERO,
            ..slack
        };
        assert_eq!(slack.me().as_deref(), Some("U00000009"));
    }
}
