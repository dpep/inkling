//! One run, start to finish.
//!
//! Two loops, kept separate on purpose. Discovery searches the window for
//! messages never seen before. Reconciliation walks every tracked record
//! that's due, however old — which is what lets a PR merging a week after its
//! message still get its signal, and what lets the search window stay small.

use crate::adapters::store_json::{Outcome, Snapshot, Store, TrackedMessage, VERSION};
use crate::config::Config;
use crate::domain::model::{Message, MessageId, ReactionName, State};
use crate::domain::reconcile::{Plan, SkipReason, admit, reconcile};
use crate::domain::schedule;
use crate::domain::window::{SearchWindow, next_window};
use crate::ports::*;
use chrono::{DateTime, Utc};

pub struct Run<'a> {
    pub config: &'a Config,
    pub source: &'a dyn MessageSource,
    pub provider: &'a dyn StatusProvider,
    pub sink: &'a dyn ReactionSink,
    pub store: &'a dyn Store,
    pub clock: &'a dyn Clock,
    pub dry_run: bool,
    /// Overrides the computed window start, for `--since`.
    pub since: Option<DateTime<Utc>>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub discovered: usize,
    pub tracked: usize,
    pub checked: usize,
    pub added: Vec<(MessageId, ReactionName)>,
    pub skipped: usize,
    pub failed: usize,
    pub terminal: usize,
    pub outcome: Option<Outcome>,
    /// Per-record detail, for dry-run output.
    pub details: Vec<Detail>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Detail {
    pub id: MessageId,
    pub permalink: String,
    pub references: Vec<String>,
    pub states: Vec<State>,
    pub existing: Option<Vec<ReactionName>>,
    pub plan: Plan,
}

impl Report {
    fn outcome(&self) -> Outcome {
        match self.outcome {
            Some(Outcome::Failure) => Outcome::Failure,
            _ if self.failed > 0 => Outcome::Partial,
            _ => Outcome::Success,
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self.outcome() {
            Outcome::Success => 0,
            Outcome::Partial => 1,
            Outcome::Failure => 2,
        }
    }
}

impl Run<'_> {
    pub fn execute(&self) -> Result<Report, StoreError> {
        let now = self.clock.now();
        let mut snapshot = self.store.load()?;
        snapshot.version = VERSION;
        snapshot.run.last_run_started_at = Some(now);

        let mut report = Report::default();
        let computed = next_window(
            &self.config.window,
            snapshot.run.last_successful_discovery_at,
            now,
        );
        let window = SearchWindow {
            start: self.since.unwrap_or(computed.start),
            ..computed
        };

        // Discovery. A failure here must not advance the watermark: a gap in
        // discovery is the one thing no later run recovers from.
        let discovery_ok = self.discover(&mut snapshot, &window, now, &mut report);

        self.reconcile_due(&mut snapshot, now, &mut report);
        self.prune(&mut snapshot, now);

        if discovery_ok {
            // Safe even on a partial run: per-record failures live in the
            // records, which get retried regardless of the window. But a
            // --since that *narrowed* the window must not advance it, or a
            // hand-run after a long sleep would silently skip the gap.
            if window.start <= computed.start {
                snapshot.run.last_successful_discovery_at = Some(window.end);
            }
        } else {
            report.outcome = Some(Outcome::Failure);
        }

        report.tracked = snapshot.tracked.len();
        report.terminal = snapshot.tracked.values().filter(|t| t.terminal).count();
        snapshot.run.last_run_outcome = Some(report.outcome());

        if !self.dry_run {
            self.store.save(&snapshot)?;
        }
        Ok(report)
    }

    /// Returns whether discovery itself succeeded.
    fn discover(
        &self,
        snapshot: &mut Snapshot,
        window: &SearchWindow,
        now: DateTime<Utc>,
        report: &mut Report,
    ) -> bool {
        if window.is_empty() {
            return true;
        }
        let mut scopes = Vec::new();
        if self.config.slack.personal {
            scopes.push(Scope::Personal);
        }
        scopes.extend(
            self.config
                .slack
                .channels
                .iter()
                .map(|c| Scope::Channel(c.clone())),
        );

        let mut ok = true;
        for scope in &scopes {
            match self.source.discover(scope, window) {
                Ok(messages) => {
                    for message in messages {
                        if self.track(snapshot, message, now, report) {
                            report.discovered += 1;
                        }
                    }
                }
                Err(error) => {
                    eprintln!("inkling: discovery failed for {scope:?}: {error}");
                    ok = false;
                }
            }
        }
        ok
    }

    /// Add a message to the tracked set, unless it's already there or the
    /// filters reject it.
    fn track(
        &self,
        snapshot: &mut Snapshot,
        message: Message,
        now: DateTime<Utc>,
        report: &mut Report,
    ) -> bool {
        let key = message.id.to_string();
        if snapshot.tracked.contains_key(&key) {
            return false;
        }
        match admit(&message, self.config) {
            Ok(references) => {
                snapshot.tracked.insert(
                    key,
                    TrackedMessage {
                        id: message.id,
                        permalink: message.permalink,
                        references,
                        last_state: None,
                        applied: Vec::new(),
                        posted_at: message.posted_at,
                        first_seen: now,
                        last_checked: None,
                        terminal: false,
                    },
                );
                true
            }
            Err(SkipReason::NoReference) => false,
            Err(_) => {
                report.skipped += 1;
                false
            }
        }
    }

    fn reconcile_due(&self, snapshot: &mut Snapshot, now: DateTime<Utc>, report: &mut Report) {
        let due: Vec<String> = snapshot
            .tracked
            .iter()
            .filter(|(_, t)| self.is_due(t, now))
            .map(|(key, _)| key.clone())
            .collect();
        if due.is_empty() {
            return;
        }

        // One reactions read per channel beats one per message; the source
        // groups internally.
        let ids: Vec<MessageId> = due
            .iter()
            .filter_map(|key| snapshot.tracked.get(key).map(|t| t.id.clone()))
            .collect();
        let known = self.source.reactions(&ids).ok();

        for key in due {
            let Some(record) = snapshot.tracked.get(&key).cloned() else {
                continue;
            };
            report.checked += 1;

            let states = match self.provider.lookup(&record.references) {
                Ok(results) => results,
                Err(error) => {
                    eprintln!("inkling: lookup failed for {}: {error}", record.id);
                    report.failed += 1;
                    continue;
                }
            };
            // A reference nobody can read leaves the message alone rather
            // than letting its siblings speak for it.
            if states.iter().any(|(_, s)| s.is_err()) {
                report.failed += 1;
                continue;
            }
            let states: Vec<State> = states.into_iter().filter_map(|(_, s)| s.ok()).collect();

            let existing = known.as_ref().and_then(|m| m.get(&record.id).cloned());
            let plan = reconcile(&states, existing.as_deref(), self.config);

            report.details.push(Detail {
                id: record.id.clone(),
                permalink: record.permalink.clone(),
                references: record.references.iter().map(ToString::to_string).collect(),
                states: states.clone(),
                existing: existing.clone(),
                plan: plan.clone(),
            });

            self.apply(snapshot, &key, &plan, now, report);
        }
    }

    fn apply(
        &self,
        snapshot: &mut Snapshot,
        key: &str,
        plan: &Plan,
        now: DateTime<Utc>,
        report: &mut Report,
    ) {
        let Some(record) = snapshot.tracked.get(key) else {
            return;
        };
        let id = record.id.clone();

        if let Some(emoji) = &plan.add {
            if self.dry_run {
                report.added.push((id.clone(), emoji.clone()));
            } else {
                match self.sink.add(&id, emoji) {
                    Ok(()) => {
                        report.added.push((id.clone(), emoji.clone()));
                        // Ledger only what we can prove we added — evidence
                        // showed it absent beforehand. Slack reports the same
                        // success for a duplicate.
                        if plan.ledger
                            && let Some(r) = snapshot.tracked.get_mut(key)
                        {
                            r.applied.push(emoji.clone());
                        }
                    }
                    Err(SinkError::MessageNotFound) => {
                        snapshot.tracked.remove(key);
                        return;
                    }
                    Err(error) => {
                        eprintln!("inkling: could not react to {id}: {error}");
                        report.failed += 1;
                    }
                }
            }
        }

        if let Some(record) = snapshot.tracked.get_mut(key) {
            record.last_checked = Some(now);
            record.last_state = plan.aggregate;
            record.terminal = plan.terminal;
        }
    }

    fn is_due(&self, record: &TrackedMessage, now: DateTime<Utc>) -> bool {
        if record.terminal {
            return false;
        }
        match record.last_checked {
            None => true,
            Some(last) => {
                now >= schedule::next_check_at(&self.config.tracking, record.posted_at, last)
            }
        }
    }

    /// Terminal records are kept and skipped until they fall out of the
    /// search window — deleting one immediately would just let discovery
    /// find it again next run.
    fn prune(&self, snapshot: &mut Snapshot, now: DateTime<Utc>) {
        let max_lookback = chrono::TimeDelta::from_std(self.config.window.max_lookback)
            .unwrap_or(chrono::TimeDelta::MAX);
        snapshot.tracked.retain(|_, record| {
            if schedule::is_expired(&self.config.tracking, record.posted_at, now) {
                return false;
            }
            !(record.terminal && now - record.posted_at > max_lookback)
        });
    }
}
