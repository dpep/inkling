//! Whole runs against fakes: what advances the watermark, what doesn't, and
//! what one broken record does to the rest.

use chrono::{TimeDelta, Utc};
use inkling::adapters::store_json::{Outcome, Snapshot, Store};
use inkling::app::Run;
use inkling::config::Config;
use inkling::domain::model::{Reference, State};
use inkling::ports::*;
use inkling::testing::*;
use std::cell::RefCell;

const CONFIG: &str = r##"
    [slack]
    personal = false
    user_id = "U_ME"
    channels = ["#inkling"]

    [status.approved]
    canonical = "white_check_mark"

    [status.merged]
    canonical = "merged"
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

#[derive(Default)]
struct MemoryStore(RefCell<Snapshot>);

impl Store for MemoryStore {
    fn load(&self) -> Result<Snapshot, StoreError> {
        Ok(self.0.borrow().clone())
    }
    fn save(&self, snapshot: &Snapshot) -> Result<(), StoreError> {
        *self.0.borrow_mut() = snapshot.clone();
        Ok(())
    }
}

/// A source whose discovery always fails, to exercise the watermark rule.
struct BrokenSource;

impl MessageSource for BrokenSource {
    fn discover(
        &self,
        _scope: &Scope,
        _window: &inkling::domain::window::SearchWindow,
    ) -> Result<Discovered, SourceError> {
        Err(SourceError::Search("slack is down".into()))
    }
    fn reactions(
        &self,
        _ids: &[inkling::domain::model::MessageId],
    ) -> Result<
        std::collections::HashMap<
            inkling::domain::model::MessageId,
            Vec<inkling::domain::model::ReactionName>,
        >,
        SourceError,
    > {
        Ok(std::collections::HashMap::new())
    }
}

fn merged_message() -> inkling::domain::model::Message {
    let mut m = message(
        "1786596050.245879",
        "test fixture — merged PR https://github.com/dpep/gqls/pull/1",
        "2026-08-12T21:20:50Z",
    );
    m.id.channel = "C_INK".into();
    m
}

#[test]
fn a_configured_channel_name_reaches_slack_as_a_name() {
    // Config takes handles, and search accepts them directly — no ID lookup
    // stands between what you write and what gets searched.
    use inkling::adapters::slack_claude::search_query;
    assert_eq!(
        search_query(&Scope::Channel("#inkling".into())),
        "in:#inkling"
    );
    assert_eq!(
        config().slack.channels,
        vec!["#inkling"],
        "the config value is a handle, and stays one"
    );
}

#[test]
fn author_names_are_resolved_and_remembered() {
    // Slack search reports usernames, not names. Resolving costs a call per
    // person, so the answer is cached rather than re-fetched every run.
    struct NamingSource(FakeMessageSource, RefCell<usize>);

    impl MessageSource for NamingSource {
        fn discover(
            &self,
            scope: &Scope,
            window: &inkling::domain::window::SearchWindow,
        ) -> Result<Discovered, SourceError> {
            self.0.discover(scope, window)
        }
        fn reactions(
            &self,
            ids: &[inkling::domain::model::MessageId],
        ) -> Result<
            std::collections::HashMap<
                inkling::domain::model::MessageId,
                Vec<inkling::domain::model::ReactionName>,
            >,
            SourceError,
        > {
            self.0.reactions(ids)
        }
        fn user_names(
            &self,
            ids: &[String],
        ) -> Result<std::collections::HashMap<String, String>, SourceError> {
            *self.1.borrow_mut() += 1;
            Ok(ids
                .iter()
                .map(|id| (id.clone(), "Daniel Pepper".to_string()))
                .collect())
        }
    }

    let config = config();
    let mut nameless = merged_message();
    nameless.author_name.clear();

    let source = NamingSource(FakeMessageSource::with(vec![nameless]), RefCell::new(0));
    let provider = ScriptedStatusProvider::new();
    provider.set(&pr("gqls", 1), State::Open);
    let sink = RecordingSink::new();
    let store = MemoryStore::default();
    let clock = FixedClock::at("2026-08-12T21:30:00Z");

    let run = || {
        Run {
            config: &config,
            source: &source,
            provider: &provider,
            sink: &sink,
            store: &store,
            clock: &clock,
            dry_run: false,
            since: None,
        }
        .execute()
        .unwrap()
    };

    let report = run();
    assert_eq!(report.details[0].author, "Daniel Pepper");
    assert_eq!(*source.1.borrow(), 1);

    run();
    assert_eq!(*source.1.borrow(), 1, "the name is cached, not re-fetched");
}

#[test]
fn a_newly_added_channel_gets_a_first_run_window() {
    // The gotcha this exists to prevent: with one shared watermark, adding a
    // channel to a setup that's been running for weeks tells it the recent
    // past is already covered, so it finds nothing until someone posts again.
    let config = config();
    let source = FakeMessageSource::with(vec![merged_message()]);
    let provider = ScriptedStatusProvider::new();
    provider.set(&pr("gqls", 1), State::Merged);
    let sink = RecordingSink::new();
    let store = MemoryStore::default();

    // A long-running install: personal has been searched up to a minute ago.
    let mut snapshot = Snapshot::default();
    let recent = "2026-08-12T21:29:00Z".parse().unwrap();
    snapshot.run.scopes.insert("personal".into(), recent);
    snapshot.run.last_successful_discovery_at = Some(recent);
    store.save(&snapshot).unwrap();

    let clock = FixedClock::at("2026-08-12T21:30:00Z");
    let report = Run {
        config: &config,
        source: &source,
        provider: &provider,
        sink: &sink,
        store: &store,
        clock: &clock,
        dry_run: false,
        since: None,
    }
    .execute()
    .unwrap();

    // The message is ~10 minutes old — inside the channel's first-run window,
    // outside the minute personal had left to cover.
    assert_eq!(report.discovered, 1);
    assert_eq!(sink.emoji_names(), vec!["merged"]);

    let saved = store.load().unwrap();
    assert!(saved.run.scopes.contains_key("channel:inkling"));
    assert_eq!(
        saved.run.scopes["personal"], recent,
        "an untouched scope keeps its own mark"
    );
}

#[test]
fn the_report_names_what_it_is_tracking() {
    // "tracking 2" is not an answer on its own — the point of the list is to
    // say which two, and what each one is.
    let config = config();
    let mut fresh = merged_message();
    fresh.posted_at = "2026-08-12T21:25:00Z".parse().unwrap();

    let source = FakeMessageSource::with(vec![fresh]);
    let provider = ScriptedStatusProvider::new();
    provider.set_with_author(&pr("gqls", 1), State::Approved, "Daniel Pepper");
    let sink = RecordingSink::new();
    let store = MemoryStore::default();
    let clock = FixedClock::at("2026-08-12T21:30:00Z");

    let report = Run {
        config: &config,
        source: &source,
        provider: &provider,
        sink: &sink,
        store: &store,
        clock: &clock,
        dry_run: false,
        since: None,
    }
    .execute()
    .unwrap();

    assert_eq!(report.tracking.len(), report.tracked);
    let tracked = &report.tracking[0];
    assert_eq!(tracked.channel, "#test");
    assert_eq!(tracked.state, Some(State::Approved));
    assert_eq!(tracked.refs[0].0, "github.com/dpep/gqls#1");
    assert!(!tracked.terminal);
    // The emoji it just applied shows against the record that carries it.
    assert_eq!(
        tracked.applied,
        vec![inkling::domain::model::ReactionName::new(
            "white_check_mark"
        )]
    );
}

#[test]
fn a_capped_search_advances_only_as_far_as_it_read() {
    // Slack caps a search page, so a wide window over a busy channel returns
    // a prefix. Advancing to the window's end would step over the remainder;
    // advancing to the newest message read leaves it for the next run.
    struct CappedSource(Vec<inkling::domain::model::Message>);

    impl MessageSource for CappedSource {
        fn discover(
            &self,
            _scope: &Scope,
            _window: &inkling::domain::window::SearchWindow,
        ) -> Result<Discovered, SourceError> {
            Ok(Discovered {
                messages: self.0.clone(),
                complete: false,
            })
        }
        fn reactions(
            &self,
            _ids: &[inkling::domain::model::MessageId],
        ) -> Result<
            std::collections::HashMap<
                inkling::domain::model::MessageId,
                Vec<inkling::domain::model::ReactionName>,
            >,
            SourceError,
        > {
            Ok(std::collections::HashMap::new())
        }
    }

    let config = config();
    let newest = merged_message();
    let covered_to = newest.posted_at;

    let store = MemoryStore::default();
    let clock = FixedClock::at("2026-08-12T21:30:00Z");
    Run {
        config: &config,
        source: &CappedSource(vec![newest]),
        provider: &ScriptedStatusProvider::new(),
        sink: &RecordingSink::new(),
        store: &store,
        clock: &clock,
        dry_run: false,
        since: None,
    }
    .execute()
    .unwrap();

    let mark = store.load().unwrap().run.scopes["channel:inkling"];
    assert_eq!(mark, covered_to, "the unread remainder stays searchable");
    assert!(mark < clock.now());
}

#[test]
fn a_failed_scope_does_not_hold_back_the_others() {
    // Watermarks are per scope, so one broken channel shouldn't make every
    // other channel re-scan the same window forever.
    let config = config();
    let store = MemoryStore::default();
    let clock = FixedClock::at("2026-08-12T21:30:00Z");

    Run {
        config: &config,
        source: &BrokenSource,
        provider: &ScriptedStatusProvider::new(),
        sink: &RecordingSink::new(),
        store: &store,
        clock: &clock,
        dry_run: false,
        since: None,
    }
    .execute()
    .unwrap();

    assert!(
        store.load().unwrap().run.scopes.is_empty(),
        "a scope that failed records no progress"
    );
}

#[test]
fn personal_mode_actually_searches() {
    // Regression: the scope carried no user id, so the adapter refused it and
    // every personal-mode run reported a discovery failure. It stayed hidden
    // while personal defaulted off.
    let mut config = config();
    config.slack.personal = true;
    config.slack.channels.clear();
    config.slack.user_id = Some("U_ME".into());

    let source = FakeMessageSource::with(vec![merged_message()]);
    let provider = ScriptedStatusProvider::new();
    provider.set(&pr("gqls", 1), State::Merged);
    let sink = RecordingSink::new();
    let store = MemoryStore::default();
    let clock = FixedClock::at("2026-08-12T21:30:00Z");

    let report = Run {
        config: &config,
        source: &source,
        provider: &provider,
        sink: &sink,
        store: &store,
        clock: &clock,
        dry_run: false,
        since: None,
    }
    .execute()
    .unwrap();

    assert!(!report.discovery_failed());
    assert_eq!(report.discovered, 1);
    assert_eq!(sink.emoji_names(), vec!["merged"]);
}

#[test]
fn a_clean_run_signals_and_advances_the_watermark() {
    let config = config();
    let source = FakeMessageSource::with(vec![merged_message()]);
    let provider = ScriptedStatusProvider::new();
    provider.set(&pr("gqls", 1), State::Merged);
    let sink = RecordingSink::new();
    let store = MemoryStore::default();
    let clock = FixedClock::at("2026-08-12T21:30:00Z");

    let report = Run {
        config: &config,
        source: &source,
        provider: &provider,
        sink: &sink,
        store: &store,
        clock: &clock,
        dry_run: false,
        since: None,
    }
    .execute()
    .unwrap();

    assert_eq!(report.discovered, 1);
    assert_eq!(sink.emoji_names(), vec!["merged"]);
    assert_eq!(report.exit_code(), 0);

    let saved = store.load().unwrap();
    assert_eq!(
        saved.run.last_successful_discovery_at,
        Some(clock.now()),
        "a clean run advances the watermark"
    );
    assert_eq!(saved.run.last_run_outcome, Some(Outcome::Success));

    // The ledger records it, because evidence showed the emoji absent first.
    let record = saved.tracked.values().next().unwrap();
    assert_eq!(record.applied.len(), 1);
    assert!(record.terminal);
}

#[test]
fn a_dry_run_changes_nothing() {
    let config = config();
    let source = FakeMessageSource::with(vec![merged_message()]);
    let provider = ScriptedStatusProvider::new();
    provider.set(&pr("gqls", 1), State::Merged);
    let sink = RecordingSink::new();
    let store = MemoryStore::default();
    let clock = FixedClock::at("2026-08-12T21:30:00Z");

    let report = Run {
        config: &config,
        source: &source,
        provider: &provider,
        sink: &sink,
        store: &store,
        clock: &clock,
        dry_run: true,
        since: None,
    }
    .execute()
    .unwrap();

    assert_eq!(report.added.len(), 1, "it still reports what it would do");
    assert!(sink.emoji_names().is_empty(), "but writes nothing to slack");
    assert_eq!(
        store.load().unwrap(),
        Snapshot::default(),
        "and nothing to disk"
    );
}

#[test]
fn a_failed_discovery_holds_the_watermark_back() {
    // A gap in discovery is the one failure no later run recovers from.
    let config = config();
    let provider = ScriptedStatusProvider::new();
    let sink = RecordingSink::new();
    let store = MemoryStore::default();
    let clock = FixedClock::at("2026-08-12T21:30:00Z");

    let report = Run {
        config: &config,
        source: &BrokenSource,
        provider: &provider,
        sink: &sink,
        store: &store,
        clock: &clock,
        dry_run: false,
        since: None,
    }
    .execute()
    .unwrap();

    assert_eq!(report.exit_code(), 2);
    // The per-record counters all read zero here, so this is the only thing
    // distinguishing a failed run from a quiet one.
    assert!(report.discovery_failed());
    assert_eq!(report.failed, 0);
    assert_eq!(store.load().unwrap().run.last_successful_discovery_at, None);
}

#[test]
fn one_unreadable_reference_does_not_stop_the_others() {
    let config = config();
    let mut good = merged_message();
    good.id.ts = "1786596050.245879".into();
    let mut bad = message(
        "1786596060.000000",
        "https://github.com/dpep/private/pull/9",
        "2026-08-12T21:21:00Z",
    );
    bad.id.channel = "C_INK".into();

    let source = FakeMessageSource::with(vec![good, bad]);
    let provider = ScriptedStatusProvider::new();
    provider.set(&pr("gqls", 1), State::Merged);
    provider.fail(&pr("private", 9), RefError::NotFound);
    let sink = RecordingSink::new();
    let store = MemoryStore::default();
    let clock = FixedClock::at("2026-08-12T21:30:00Z");

    let report = Run {
        config: &config,
        source: &source,
        provider: &provider,
        sink: &sink,
        store: &store,
        clock: &clock,
        dry_run: false,
        since: None,
    }
    .execute()
    .unwrap();

    assert_eq!(sink.emoji_names(), vec!["merged"], "the good one still ran");
    assert_eq!(report.failed, 1);
    assert_eq!(report.exit_code(), 1, "partial");
    assert_eq!(
        store.load().unwrap().run.last_successful_discovery_at,
        Some(clock.now()),
        "discovery succeeded, so the window still advances — the broken \
         record retries on its own schedule"
    );
}

#[test]
fn a_second_run_over_unchanged_state_does_nothing() {
    let config = config();
    let source = FakeMessageSource::with(vec![merged_message()]);
    let provider = ScriptedStatusProvider::new();
    provider.set(&pr("gqls", 1), State::Merged);
    let sink = RecordingSink::new();
    let store = MemoryStore::default();
    let clock = FixedClock::at("2026-08-12T21:30:00Z");

    let run = |clock: &FixedClock| {
        Run {
            config: &config,
            source: &source,
            provider: &provider,
            sink: &sink,
            store: &store,
            clock,
            dry_run: false,
            since: None,
        }
        .execute()
        .unwrap()
    };

    run(&clock);
    assert_eq!(sink.emoji_names().len(), 1);

    clock.advance(TimeDelta::hours(1));
    let second = run(&clock);
    assert_eq!(
        sink.emoji_names().len(),
        1,
        "merged is terminal; no re-check"
    );
    assert_eq!(second.checked, 0);
}

#[test]
fn a_narrowing_since_does_not_advance_the_watermark() {
    // Otherwise `--since 6h` after a long sleep would silently skip the gap.
    let config = config();
    let source = FakeMessageSource::with(vec![]);
    let provider = ScriptedStatusProvider::new();
    let sink = RecordingSink::new();
    let store = MemoryStore::default();

    let mut snapshot = Snapshot::default();
    let long_ago = "2026-08-10T00:00:00Z".parse().unwrap();
    snapshot
        .run
        .scopes
        .insert("channel:inkling".into(), long_ago);
    snapshot.run.last_successful_discovery_at = Some(long_ago);
    store.save(&snapshot).unwrap();

    let clock = FixedClock::at("2026-08-12T21:30:00Z");
    Run {
        config: &config,
        source: &source,
        provider: &provider,
        sink: &sink,
        store: &store,
        clock: &clock,
        dry_run: false,
        since: Some(Utc::now().max(clock.now()) - TimeDelta::hours(1)),
    }
    .execute()
    .unwrap();

    assert_eq!(
        store.load().unwrap().run.scopes["channel:inkling"],
        long_ago,
        "the un-searched gap must stay in the next window"
    );
}

#[test]
fn an_expired_message_stops_being_tracked() {
    let config = config();
    let source = FakeMessageSource::with(vec![merged_message()]);
    let provider = ScriptedStatusProvider::new();
    provider.set(&pr("gqls", 1), State::Open);
    let sink = RecordingSink::new();
    let store = MemoryStore::default();
    let clock = FixedClock::at("2026-08-12T21:30:00Z");

    let run = |clock: &FixedClock| {
        Run {
            config: &config,
            source: &source,
            provider: &provider,
            sink: &sink,
            store: &store,
            clock,
            dry_run: false,
            since: None,
        }
        .execute()
        .unwrap()
    };

    run(&clock);
    assert_eq!(store.load().unwrap().tracked.len(), 1);

    // Past the tracking horizon, nobody is coming back to read the message.
    clock.advance(TimeDelta::days(8));
    let report = run(&clock);
    assert_eq!(report.tracked, 0);
}
