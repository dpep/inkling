//! Fold one reading into a watch store, exactly as a poll does.
//!
//! The push path needs a transition to carry, and a transition needs
//! something upstream to move. Waiting on somebody else's CI makes the
//! demonstration a matter of luck, and a fake provider inside `inkling`
//! itself would be a test seam shipped to users. So: a separate binary that
//! calls the same `record` the poller calls, against a real store.
//!
//!   cargo run --example record_reading -- <watches.json> <reference> <state> [checks]
//!
//! `state` is draft|open|approved|merged|closed and `checks` is
//! pending|success|failure, or omitted for a repository that runs none.

use chrono::Utc;
use inkling::adapters::watches_json::WatchStore;
use inkling::domain::detect::{Target, parse_target};
use inkling::domain::model::{Reference, State};
use inkling::domain::wait::CheckState;
use inkling::domain::watch::{FailedCheck, Sighting, Standing};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [path, reference, state, checks @ ..] = args.as_slice() else {
        eprintln!("usage: record_reading <watches.json> <reference> <state> [checks]");
        std::process::exit(2);
    };

    let reference = resolve(reference);
    let sighting = Sighting {
        standing: Standing::PullRequest {
            state: parse_state(state),
            checks: checks.first().map(|c| parse_checks(c)),
        },
        title: "a title nobody should see in a pushed message".into(),
        head: None,
        failing: match checks.first().map(String::as_str) {
            Some("failure") => Some(FailedCheck {
                name: "test / build".into(),
                job: None,
            }),
            _ => None,
        },
    };

    let store = WatchStore::new(path);
    let now = Utc::now();
    let events = store
        .update(now, |watches| watches.record(&[(reference, sighting)], now))
        .expect("recorded");
    for event in events {
        println!(
            "seq {} {} {} → {}",
            event.seq, event.axis, event.from, event.to
        );
    }
}

fn resolve(argument: &str) -> Reference {
    match parse_target(argument, &["github.com".to_string()]) {
        Ok(Target::Reference(reference)) => reference,
        Ok(Target::Partial {
            owner: Some(owner),
            repo: Some(repo),
            number,
        }) => Reference::GitHubPullRequest {
            host: "github.com".into(),
            owner,
            repo,
            number,
        },
        _ => {
            eprintln!("{argument}: give a full reference, e.g. owner/repo#12");
            std::process::exit(2);
        }
    }
}

fn parse_state(word: &str) -> State {
    match word {
        "draft" => State::Draft,
        "open" => State::Open,
        "approved" => State::Approved,
        "merged" => State::Merged,
        "closed" => State::Closed,
        other => {
            eprintln!("{other}: not a state");
            std::process::exit(2);
        }
    }
}

fn parse_checks(word: &str) -> CheckState {
    match word {
        "pending" => CheckState::Pending,
        "success" => CheckState::Success,
        "failure" => CheckState::Failure,
        other => {
            eprintln!("{other}: not a check state");
            std::process::exit(2);
        }
    }
}
