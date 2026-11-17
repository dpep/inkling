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

use crate::adapters::slack_claude::{ActivityError, Runner, SlackClaude};
use crate::domain::model::Reference;
use crate::domain::slack::later;
use crate::ports::{MessageLookup, MessageProvider, MessageStatus, ProviderError, RefError};
use std::cell::OnceCell;

pub struct SlackThreads<R: Runner> {
    slack: SlackClaude<R>,
    /// Asked once, and only once a message is actually read: finding out who
    /// the credentials belong to may itself be a bridge call.
    identify: Box<dyn Fn() -> Option<String>>,
    me: OnceCell<Option<String>>,
}

impl<R: Runner> SlackThreads<R> {
    pub fn new(slack: SlackClaude<R>, identify: impl Fn() -> Option<String> + 'static) -> Self {
        Self {
            slack,
            identify: Box::new(identify),
            me: OnceCell::new(),
        }
    }

    fn read_one(&self, reference: &Reference) -> Result<MessageStatus, RefError> {
        let Reference::SlackMessage {
            channel,
            ts,
            thread,
        } = reference
        else {
            return Err(RefError::Other("not a slack message".into()));
        };
        let root = thread.as_deref().unwrap_or(ts);
        let entries = self
            .slack
            .activity(channel, root, ts)
            .map_err(|error| match error {
                ActivityError::Gone(_) => RefError::NotFound,
                ActivityError::Failed(why) => RefError::Other(why),
            })?;

        let mut status = MessageStatus::default();
        for entry in entries {
            if !entry.user_id.is_empty() && !entry.user_name.is_empty() {
                status
                    .names
                    .insert(entry.user_id.clone(), entry.user_name.clone());
            }
            if entry.ts == *ts {
                status.reactions = entry.reactions;
            } else if later(&entry.ts, ts) {
                // A reply's replies are the ones after it: a thread is one
                // conversation, and what came before was already there.
                status.replies.insert(entry.ts, entry.user_id);
            }
        }
        Ok(status)
    }
}

impl<R: Runner> MessageProvider for SlackThreads<R> {
    /// One call per message: the thread tool reads one thread, and a channel
    /// read would carry neither replies' authors nor anything outside its page.
    fn read(&self, messages: &[Reference]) -> Result<Vec<MessageLookup>, ProviderError> {
        Ok(messages
            .iter()
            .map(|reference| (reference.clone(), self.read_one(reference)))
            .collect())
    }

    fn me(&self) -> Option<String> {
        self.me.get_or_init(|| (self.identify)()).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::slack_claude::{REAL_THREAD, RunnerError};
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
        (SlackThreads::new(slack, || Some("U9".into())), prompts)
    }

    fn message(channel: &str, ts: &str, thread: Option<&str>) -> Reference {
        Reference::SlackMessage {
            channel: channel.into(),
            ts: ts.into(),
            thread: thread.map(Into::into),
        }
    }

    #[test]
    fn a_real_read_becomes_reactions_and_no_replies() {
        let (slack, _) = threads(&[REAL_THREAD]);
        let watched = message("D1MG8KWNR", "1771013011.519909", None);
        let read = slack.read(std::slice::from_ref(&watched)).unwrap();
        let status = read[0].1.clone().unwrap();
        assert_eq!(
            status.reactions,
            BTreeMap::from([("low-priority".into(), 1), ("problem-solved".into(), 1)])
        );
        assert!(status.replies.is_empty());
        assert_eq!(
            status.names.get("U0E48AHQA").map(String::as_str),
            Some("dpepper")
        );
    }

    const THREAD: &str = r#"{"messages":[
        {"ts":"1786596000.123456","user_id":"U9","user_name":"dpepper","reactions":{}},
        {"ts":"1786596050.245879","user_id":"U1","user_name":"alice","reactions":{"eyes":1}},
        {"ts":"1786596060.000001","user_id":"U2","user_name":"bob","reactions":{}}
    ]}"#;

    #[test]
    fn a_reply_is_read_from_its_root_and_counts_only_what_came_after_it() {
        let (slack, prompts) = threads(&[THREAD]);
        let reply = message("C0123ABCD", "1786596050.245879", Some("1786596000.123456"));
        let status = slack.read(&[reply]).unwrap()[0].1.clone().unwrap();

        assert!(prompts.borrow()[0].contains(r#"message_ts: "1786596000.123456""#));
        assert_eq!(status.reactions, BTreeMap::from([("eyes".into(), 1)]));
        assert_eq!(
            status.replies,
            BTreeMap::from([("1786596060.000001".into(), "U2".into())])
        );
    }

    #[test]
    fn the_prompt_never_asks_for_what_a_message_says() {
        let (slack, prompts) = threads(&[THREAD]);
        let _ = slack.read(&[message("C0123ABCD", "1786596000.123456", None)]);
        let prompt = &prompts.borrow()[0];
        assert!(prompt.contains("Do not include message text"), "{prompt}");
        assert!(!prompt.contains("\"text\""), "{prompt}");
    }

    #[test]
    fn a_read_that_drops_the_watched_message_is_retried() {
        let dropped = r#"{"messages":[{"ts":"1786596060.000001","user_id":"U2","reactions":{}}]}"#;
        let (slack, prompts) = threads(&[dropped, THREAD]);
        let read = slack.read(&[message("C0123ABCD", "1786596000.123456", None)]);
        assert!(read.unwrap()[0].1.is_ok());
        assert_eq!(prompts.borrow().len(), 2);
    }

    #[test]
    fn a_thread_slack_says_is_gone_is_not_found_and_not_retried() {
        let (slack, prompts) = threads(&["execution_failed: thread_not_found", THREAD]);
        let read = slack.read(&[message("C0123ABCD", "1786596000.123456", None)]);
        assert_eq!(read.unwrap()[0].1, Err(RefError::NotFound));
        assert_eq!(prompts.borrow().len(), 1);
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
            Some("U9".into())
        });
        assert_eq!(asked.get(), 0);
        assert_eq!(slack.me().as_deref(), Some("U9"));
        assert_eq!(slack.me().as_deref(), Some("U9"));
        assert_eq!(asked.get(), 1);
    }
}
