//! The whole arc, run against fakes: a PR is posted, gets approved, then
//! merges, and the message accumulates exactly the right signals along the
//! way — no more, no fewer, in the right order.
//!
//! These use the same two real messages the Slack spike found in `#coding`,
//! because between them they cover both interesting outcomes: one where the
//! correct behavior is to do nothing, and one where it is to react.

use chrono::TimeDelta;
use inkling::config::Config;
use inkling::domain::model::{Reference, State};
use inkling::domain::reconcile::{admit, reconcile};
use inkling::domain::schedule;
use inkling::domain::window::next_window;
use inkling::ports::*;
use inkling::testing::*;

const CONFIG: &str = r##"
    [slack]
    personal = true
    user_id = "U_ME"
    channels = ["#inkling"]

    [status.approved]
    canonical = "white_check_mark"
    aliases = ["lgtm"]

    [status.merged]
    canonical = "merged"
    aliases = ["shipit"]
"##;

fn config() -> Config {
    Config::parse(CONFIG, "test.toml").unwrap()
}

fn pr(repo: &str, number: u64) -> Reference {
    Reference::GitHubPullRequest {
        host: "github.com".into(),
        owner: "dpep".into(),
        repo: repo.into(),
        number,
    }
}

/// One reconcile pass over one message: look up its references, decide, write.
/// This is the shape `app.rs` will formalize in M9.
fn run_once(
    source: &FakeMessageSource,
    provider: &ScriptedStatusProvider,
    sink: &RecordingSink,
    config: &Config,
    message: &inkling::domain::model::Message,
) -> inkling::domain::reconcile::Plan {
    let references = admit(message, config).expect("message should be tracked");
    let looked_up = provider.lookup(&references).expect("provider available");
    let states: Vec<State> = looked_up
        .into_iter()
        .filter_map(|(_, s)| s.ok().map(|status| status.state))
        .collect();

    let known = source.reactions(std::slice::from_ref(&message.id)).ok();
    let reactions = known.and_then(|m| m.get(&message.id).cloned());

    let plan = reconcile(&states, reactions.as_deref(), config);
    if let Some(emoji) = &plan.add {
        sink.add(&message.id, emoji).expect("sink available");
        // Mirror the write back so the next run sees it, as Slack would.
        let mut current = reactions.unwrap_or_default();
        current.push(emoji.clone());
        let names: Vec<&str> = current.iter().map(|r| r.as_str()).collect();
        source.set_reactions(&message.id, &names);
    }
    plan
}

#[test]
fn a_pull_request_accumulates_signals_as_it_moves_through_its_lifecycle() {
    let config = config();
    let message = message(
        "1786594876.682179",
        "how about <https://github.com/dpep/gqls/pull/1|this PR>?",
        "2026-08-12T21:21:16Z",
    );
    let reference = pr("gqls", 1);

    let source = FakeMessageSource::with(vec![message.clone()]);
    let provider = ScriptedStatusProvider::new();
    let sink = RecordingSink::new();

    // Run 1 — open and unreviewed. The right answer is silence.
    provider.set(&reference, State::Open);
    let plan = run_once(&source, &provider, &sink, &config, &message);
    assert_eq!(plan.add, None);
    assert!(!plan.terminal);
    assert!(sink.emoji_names().is_empty());

    // Run 2 — approved.
    provider.set(&reference, State::Approved);
    let plan = run_once(&source, &provider, &sink, &config, &message);
    assert!(plan.ledger, "evidence was known, so the add is ours");
    assert_eq!(sink.emoji_names(), vec!["white_check_mark"]);

    // Run 3 — nothing changed. Idempotent: no second add.
    let plan = run_once(&source, &provider, &sink, &config, &message);
    assert_eq!(plan.add, None);
    assert_eq!(sink.emoji_names(), vec!["white_check_mark"]);

    // Run 4 — merged. The approval stays as history; merged joins it.
    provider.set(&reference, State::Merged);
    let plan = run_once(&source, &provider, &sink, &config, &message);
    assert!(plan.terminal, "stop polling once it lands");
    assert_eq!(sink.emoji_names(), vec!["white_check_mark", "merged"]);

    // Run 5 — still merged, still nothing to do.
    run_once(&source, &provider, &sink, &config, &message);
    assert_eq!(sink.emoji_names(), vec!["white_check_mark", "merged"]);
}

#[test]
fn a_pull_request_discovered_after_it_merged_gets_only_the_merged_signal() {
    let config = config();
    let message = message(
        "1786594876.682179",
        "https://github.com/dpep/gqls/pull/1",
        "2026-08-12T21:21:16Z",
    );
    let source = FakeMessageSource::with(vec![message.clone()]);
    let provider = ScriptedStatusProvider::new();
    let sink = RecordingSink::new();

    provider.set(&pr("gqls", 1), State::Merged);
    run_once(&source, &provider, &sink, &config, &message);

    // No replaying of the approval it must have passed through on the way.
    assert_eq!(sink.emoji_names(), vec!["merged"]);
}

#[test]
fn an_open_pull_request_is_never_touched() {
    // The polyid message from the spike: open, unapproved, and correctly
    // ignored. A system that reacted to everything would look like it worked.
    let config = config();
    let message = message(
        "1786594063.746049",
        "wonder how <https://github.com/dpep/polyid/pull/43|polyid#43> is doing?",
        "2026-08-12T21:07:43Z",
    );
    let source = FakeMessageSource::with(vec![message.clone()]);
    let provider = ScriptedStatusProvider::new();
    let sink = RecordingSink::new();

    provider.set(&pr("polyid", 43), State::Open);
    for _ in 0..3 {
        run_once(&source, &provider, &sink, &config, &message);
    }
    assert!(sink.emoji_names().is_empty());
}

#[test]
fn a_human_who_signaled_first_keeps_inkling_quiet() {
    let config = config();
    let message = message(
        "1786594876.682179",
        "https://github.com/dpep/gqls/pull/1",
        "2026-08-12T21:21:16Z",
    );
    let source = FakeMessageSource::with(vec![message.clone()]);
    source.set_reactions(&message.id, &["lgtm"]);

    let provider = ScriptedStatusProvider::new();
    let sink = RecordingSink::new();
    provider.set(&pr("gqls", 1), State::Approved);

    run_once(&source, &provider, &sink, &config, &message);
    assert!(sink.emoji_names().is_empty(), "an alias counts as evidence");
}

#[test]
fn an_unreadable_reference_leaves_its_message_alone() {
    let config = config();
    let message = message(
        "1786594876.682179",
        "https://github.com/dpep/private/pull/9",
        "2026-08-12T21:21:16Z",
    );
    let source = FakeMessageSource::with(vec![message.clone()]);
    let provider = ScriptedStatusProvider::new();
    let sink = RecordingSink::new();

    provider.fail(&pr("private", 9), RefError::NotFound);
    let plan = run_once(&source, &provider, &sink, &config, &message);

    assert_eq!(plan.aggregate, None, "nothing to go on");
    assert!(sink.emoji_names().is_empty());
    assert!(!plan.terminal, "retry next run rather than giving up");
}

#[test]
fn a_message_whose_reactions_cannot_be_read_is_still_signaled() {
    let config = config();
    let message = message(
        "1786594876.682179",
        "https://github.com/dpep/gqls/pull/1",
        "2026-08-12T21:21:16Z",
    );
    let mut source = FakeMessageSource::with(vec![message.clone()]);
    source.reactions_unavailable = true;

    let provider = ScriptedStatusProvider::new();
    let sink = RecordingSink::new();
    provider.set(&pr("gqls", 1), State::Merged);

    let plan = run_once(&source, &provider, &sink, &config, &message);

    // Adding is safe — a duplicate is harmless — but the add can't be claimed
    // as ours, because we never saw that it was absent.
    assert_eq!(sink.emoji_names(), vec!["merged"]);
    assert!(!plan.ledger);
}

#[test]
fn discovery_finds_only_messages_inside_the_window() {
    let config = config();
    let old = message(
        "1786500000.000000",
        "https://github.com/dpep/gqls/pull/1",
        "2026-08-11T00:00:00Z",
    );
    let fresh = message(
        "1786594876.682179",
        "https://github.com/dpep/gqls/pull/2",
        "2026-08-12T21:21:16Z",
    );
    let source = FakeMessageSource::with(vec![old, fresh.clone()]);

    let now = "2026-08-12T21:30:00Z".parse().unwrap();
    let last_run = Some("2026-08-12T21:00:00Z".parse().unwrap());
    let window = next_window(&config.window, last_run, now);

    let found = source
        .discover(&Scope::Personal("U_ME".into()), &window)
        .unwrap();
    assert_eq!(found, vec![fresh]);
}

#[test]
fn an_aging_message_is_checked_less_often() {
    let config = config();
    let posted = "2026-08-12T00:00:00Z".parse().unwrap();

    let fresh = schedule::next_check_at(&config.tracking, posted, posted);
    assert_eq!(fresh - posted, TimeDelta::minutes(20));

    let old = posted + TimeDelta::days(3);
    let later = schedule::next_check_at(&config.tracking, posted, old);
    assert_eq!(later - old, TimeDelta::minutes(160));

    assert!(!schedule::is_expired(&config.tracking, posted, old));
    assert!(schedule::is_expired(
        &config.tracking,
        posted,
        posted + TimeDelta::days(7)
    ));
}
