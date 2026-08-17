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
    ) -> Result<Vec<inkling::domain::model::Message>, SourceError> {
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
        store.load().unwrap().run.last_successful_discovery_at,
        Some(long_ago),
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
