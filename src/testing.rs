//! Fakes standing in for GitHub and the clock, so a pull request can be
//! walked through its lifecycle without a network.

use crate::domain::model::{Reference, State};
use crate::domain::wait::CheckState;
use crate::ports::*;
use chrono::{DateTime, Utc};
use std::cell::RefCell;
use std::collections::HashMap;

pub struct FixedClock(pub RefCell<DateTime<Utc>>);

impl FixedClock {
    pub fn at(t: &str) -> Self {
        Self(RefCell::new(t.parse().expect("valid timestamp")))
    }

    pub fn advance(&self, by: chrono::TimeDelta) {
        *self.0.borrow_mut() += by;
    }
}

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.borrow()
    }
}

/// A provider whose answers can be rewritten between runs, so a test can walk
/// a PR from open to approved to merged and assert what each run does.
#[derive(Default)]
pub struct ScriptedStatusProvider {
    states: RefCell<HashMap<Reference, Result<RefStatus, RefError>>>,
    lookups: RefCell<usize>,
}

impl ScriptedStatusProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, reference: &Reference, state: State) {
        self.states
            .borrow_mut()
            .insert(reference.clone(), Ok(RefStatus::new(state)));
    }

    /// For waits on CI, where the lifecycle state is beside the point.
    pub fn set_checks(&self, reference: &Reference, state: State, checks: CheckState) {
        self.states.borrow_mut().insert(
            reference.clone(),
            Ok(RefStatus {
                checks: Some(checks),
                ..RefStatus::new(state)
            }),
        );
    }

    /// For watches on CI, where naming the red job is the point.
    pub fn fail_check(&self, reference: &Reference, state: State, failing: &str) {
        self.states.borrow_mut().insert(
            reference.clone(),
            Ok(RefStatus {
                checks: Some(CheckState::Failure),
                failing: Some(crate::domain::watch::FailedCheck {
                    name: failing.to_string(),
                    job: None,
                }),
                ..RefStatus::new(state)
            }),
        );
    }

    /// How many times the provider was actually asked — the measurement
    /// deduplicated polling exists to keep down.
    pub fn calls(&self) -> usize {
        *self.lookups.borrow()
    }

    pub fn fail(&self, reference: &Reference, error: RefError) {
        self.states
            .borrow_mut()
            .insert(reference.clone(), Err(error));
    }
}

impl StatusProvider for ScriptedStatusProvider {
    fn lookup(&self, references: &[Reference]) -> Result<Vec<Lookup>, ProviderError> {
        *self.lookups.borrow_mut() += 1;
        let states = self.states.borrow();
        Ok(references
            .iter()
            .map(|r| {
                let state = states.get(r).cloned().unwrap_or(Err(RefError::NotFound));
                (r.clone(), state)
            })
            .collect())
    }
}
