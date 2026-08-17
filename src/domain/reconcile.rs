//! Deciding what a message should be signaling, and what to do about it.
//!
//! Two steps, deliberately separate. `admit` runs once when a message is
//! discovered and decides whether it's worth tracking at all. `reconcile`
//! runs every time a tracked record comes due and decides what to add.
//!
//! Both are pure functions over data. This is the heart of the system and the
//! primary thing under test — no Slack, no GitHub, no clock.

use crate::config::Config;
use crate::domain::detect::detect;
use crate::domain::model::{Message, ReactionName, Reference, State};

/// Why a message isn't worth tracking, or a record isn't worth acting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Nothing recognizable to look up.
    NoReference,
    /// Long enough that it's a digest or a standup post; one reaction on it
    /// would say nothing useful.
    TooLong,
    TooManyReferences,
    /// Someone else wrote it, and `only_my_messages` is on.
    NotMyMessage,
}

/// What to do with a tracked record right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The least-advanced state across the message's references — how far
    /// along the message as a whole is. `None` when there's nothing to go on.
    pub aggregate: Option<State>,
    /// The emoji this state calls for, whether or not it's already there.
    pub desired: Option<ReactionName>,
    /// The emoji to actually add. `None` means nothing to do.
    pub add: Option<ReactionName>,
    /// The reaction already on the message that made an add unnecessary.
    /// Often an alias rather than the canonical one, which is why saying
    /// "canonical already present" would be wrong.
    pub satisfied_by: Option<ReactionName>,
    /// Whether adding it may be recorded as Inkling's own.
    ///
    /// Only true when evidence was known and showed the emoji absent — the
    /// Slack API returns the same success for a duplicate as for a real add,
    /// so ownership can never be inferred afterward. False here means the
    /// emoji is added but never claimed, and so never retracted later.
    pub ledger: bool,
    /// Every reference has reached a terminal state; stop polling.
    pub terminal: bool,
}

impl Plan {
    fn nothing() -> Self {
        Self {
            aggregate: None,
            desired: None,
            add: None,
            satisfied_by: None,
            ledger: false,
            terminal: false,
        }
    }
}

/// Decide whether a freshly discovered message is worth tracking, and if so,
/// what it references.
pub fn admit(message: &Message, config: &Config) -> Result<Vec<Reference>, SkipReason> {
    if config.filters.only_my_messages
        && config.slack.user_id.as_deref() != Some(message.author.as_str())
    {
        return Err(SkipReason::NotMyMessage);
    }
    if message.text.chars().count() > config.filters.max_message_chars {
        return Err(SkipReason::TooLong);
    }

    let references = detect(&message.text, &config.github.hosts);
    if references.is_empty() {
        return Err(SkipReason::NoReference);
    }
    if references.len() > config.filters.max_references_per_message {
        return Err(SkipReason::TooManyReferences);
    }
    Ok(references)
}

/// Work out what a record should be signaling, given the current state of
/// each of its references and whatever reactions the message already carries.
///
/// `reactions` is `None` when the source couldn't determine them this run —
/// which is different from "there are none" and is handled differently.
pub fn reconcile(states: &[State], reactions: Option<&[ReactionName]>, config: &Config) -> Plan {
    // The message is only as far along as its least-advanced reference. Two
    // merged PRs read as merged; one merged and one still open does not,
    // because the reaction lands on the message, not on a link. Closed sorts
    // lowest, so a message holding a corpse says nothing rather than
    // advertising the half that succeeded.
    let Some(&aggregate) = states.iter().min() else {
        return Plan::nothing();
    };

    // Done when the *aggregate* is terminal, which is stronger than "every
    // reference is terminal". Closed sorts lowest, so one closed PR pins the
    // minimum there permanently — the sibling PR may still be live, but
    // nothing it does can ever change what this message signals.
    let terminal = aggregate.is_terminal();
    let desired = config.canonical_for(aggregate);
    let Some(emoji) = desired.clone() else {
        return Plan {
            aggregate: Some(aggregate),
            terminal,
            ..Plan::nothing()
        };
    };

    // Any of this state's emoji counts, from anyone: if a human already
    // signaled it, Inkling has nothing to add.
    let evidence = config.evidence_for(aggregate);
    let satisfied_by = reactions
        .and_then(|present| present.iter().find(|r| evidence.contains(r)))
        .cloned();

    let (add, ledger) = match (reactions.is_some(), satisfied_by.is_some()) {
        (_, true) => (None, false),
        (true, false) => (Some(emoji), true),
        // Unknown: adding is safe because duplicates are harmless, but the
        // add can't be claimed as ours without having seen it was absent.
        (false, false) => (Some(emoji), false),
    };

    Plan {
        aggregate: Some(aggregate),
        desired,
        add,
        satisfied_by,
        ledger,
        terminal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    const CONFIG: &str = r##"
        [slack]
        personal = true
        user_id = "U_ME"
        channels = ["#coding"]

        [status.approved]
        canonical = "white_check_mark"
        aliases = ["lgtm", "heavy_check_mark"]

        [status.merged]
        canonical = "merged"
        aliases = ["shipit"]
    "##;

    fn config() -> Config {
        Config::parse(CONFIG, "test.toml").unwrap()
    }

    fn message(text: &str) -> Message {
        Message {
            id: crate::domain::model::MessageId::new("C1", "1786594063.746049"),
            channel_name: "coding".to_string(),
            text: text.to_string(),
            author: "U_ME".to_string(),
            author_name: "me".to_string(),
            posted_at: "2026-08-12T21:07:43Z".parse::<DateTime<Utc>>().unwrap(),
            permalink: "https://example.slack.com/archives/C1/p1786594063746049".to_string(),
            reactions: None,
        }
    }

    fn reactions(names: &[&str]) -> Vec<ReactionName> {
        names.iter().map(ReactionName::new).collect()
    }

    fn plan(states: &[State], present: Option<&[&str]>) -> Plan {
        let present = present.map(reactions);
        reconcile(states, present.as_deref(), &config())
    }

    // ── admit ───────────────────────────────────────────────────────────────

    #[test]
    fn a_message_with_a_pull_request_is_tracked() {
        let msg = message("how about <https://github.com/dpep/gqls/pull/1|this PR>?");
        assert_eq!(admit(&msg, &config()).unwrap().len(), 1);
    }

    #[test]
    fn a_message_without_a_reference_is_skipped() {
        let msg = message("just chatting about pull requests");
        assert_eq!(admit(&msg, &config()), Err(SkipReason::NoReference));
    }

    #[test]
    fn an_overlong_message_is_skipped() {
        let mut msg = message("https://github.com/dpep/inkling/pull/1 ");
        msg.text.push_str(&"padding ".repeat(200));
        assert_eq!(admit(&msg, &config()), Err(SkipReason::TooLong));
    }

    #[test]
    fn a_message_stuffed_with_references_is_skipped() {
        let links: Vec<_> = (1..=5)
            .map(|n| format!("https://github.com/dpep/inkling/pull/{n}"))
            .collect();
        let msg = message(&links.join(" "));
        assert_eq!(admit(&msg, &config()), Err(SkipReason::TooManyReferences));
    }

    #[test]
    fn another_authors_message_is_skipped_when_scoped_to_mine() {
        let mut msg = message("https://github.com/dpep/inkling/pull/1");
        msg.author = "U_SOMEONE_ELSE".to_string();
        assert_eq!(admit(&msg, &config()), Err(SkipReason::NotMyMessage));
    }

    #[test]
    fn another_authors_message_is_tracked_once_the_filter_is_off() {
        let mut config = config();
        config.filters.only_my_messages = false;
        let mut msg = message("https://github.com/dpep/inkling/pull/1");
        msg.author = "U_SOMEONE_ELSE".to_string();
        assert!(admit(&msg, &config).is_ok());
    }

    // ── reconcile ───────────────────────────────────────────────────────────

    #[test]
    fn an_open_pull_request_signals_nothing() {
        // The most valuable case: doing nothing is correct, and a system that
        // reacted to everything would look like it worked.
        let plan = plan(&[State::Open], Some(&[]));
        assert_eq!(plan.aggregate, Some(State::Open));
        assert_eq!(plan.add, None);
        assert!(!plan.terminal);
    }

    #[test]
    fn an_approved_pull_request_gets_the_approved_emoji() {
        let plan = plan(&[State::Approved], Some(&[]));
        assert_eq!(plan.add, Some(ReactionName::new("white_check_mark")));
        assert!(plan.ledger);
        assert!(!plan.terminal);
    }

    #[test]
    fn a_merged_pull_request_gets_only_the_merged_emoji() {
        // Discovered cold, already merged: no replaying of the approval it
        // must have passed through.
        let plan = plan(&[State::Merged], Some(&[]));
        assert_eq!(plan.add, Some(ReactionName::new("merged")));
        assert!(plan.terminal);
    }

    #[test]
    fn a_closed_pull_request_signals_nothing_but_is_terminal() {
        let plan = plan(&[State::Closed], Some(&[]));
        assert_eq!(plan.add, None);
        assert!(plan.terminal, "closed needs no further polling");
    }

    #[test]
    fn an_existing_canonical_reaction_suppresses_the_add() {
        let plan = plan(&[State::Merged], Some(&["merged"]));
        assert_eq!(plan.desired, Some(ReactionName::new("merged")));
        assert_eq!(plan.add, None);
    }

    #[test]
    fn an_alias_someone_else_used_counts_as_evidence() {
        let plan = plan(&[State::Approved], Some(&["lgtm"]));
        assert_eq!(plan.add, None, "a human already signaled it");
        // Naming the canonical would claim a reaction that isn't there.
        assert_eq!(plan.satisfied_by, Some(ReactionName::new("lgtm")));
        assert_eq!(plan.desired, Some(ReactionName::new("white_check_mark")));
    }

    #[test]
    fn an_unrelated_reaction_is_not_evidence() {
        let plan = plan(&[State::Approved], Some(&["tada", "robot_face"]));
        assert_eq!(plan.add, Some(ReactionName::new("white_check_mark")));
    }

    #[test]
    fn unknown_reactions_still_add_but_claim_nothing() {
        // Slack returns the same success for a duplicate add as a real one,
        // so an unverified add must never be recorded as ours.
        let plan = plan(&[State::Approved], None);
        assert_eq!(plan.add, Some(ReactionName::new("white_check_mark")));
        assert!(!plan.ledger);
    }

    #[test]
    fn the_message_is_only_as_far_along_as_its_slowest_reference() {
        let plan = plan(&[State::Merged, State::Open], Some(&[]));
        assert_eq!(plan.aggregate, Some(State::Open));
        assert_eq!(plan.add, None);
        assert!(!plan.terminal, "one reference is still live");
    }

    #[test]
    fn several_merged_references_signal_merged() {
        let plan = plan(&[State::Merged, State::Merged], Some(&[]));
        assert_eq!(plan.add, Some(ReactionName::new("merged")));
        assert!(plan.terminal);
    }

    #[test]
    fn an_approved_reference_alongside_a_closed_one_says_nothing() {
        // Half the message is dead; claiming the approval would mislead.
        let plan = plan(&[State::Approved, State::Closed], Some(&[]));
        assert_eq!(plan.aggregate, Some(State::Closed));
        assert_eq!(plan.add, None);
        // And stop polling: the live sibling can still move, but the closed
        // one pins the aggregate at Closed no matter what it does.
        assert!(plan.terminal);
    }

    #[test]
    fn no_states_means_nothing_to_do() {
        assert_eq!(plan(&[], Some(&[])), Plan::nothing());
    }

    #[test]
    fn reconciling_is_idempotent() {
        // Apply the first plan, then reconcile again: nothing left to do.
        let first = plan(&[State::Merged], Some(&[]));
        let applied = first.add.clone().unwrap();
        let second = reconcile(&[State::Merged], Some(&[applied]), &config());
        assert_eq!(second.add, None);
    }

    #[test]
    fn a_disabled_state_is_recognized_but_never_drawn() {
        let raw =
            format!("{CONFIG}\n[status.closed]\ncanonical = \"wastebasket\"\nenabled = false\n");
        let config = Config::parse(&raw, "test.toml").unwrap();
        let plan = reconcile(&[State::Closed], Some(&[]), &config);
        assert_eq!(plan.aggregate, Some(State::Closed));
        assert_eq!(plan.add, None);
    }
}
