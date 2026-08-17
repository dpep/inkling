//! One run, start to finish.
//!
//! Two loops, kept separate on purpose. Discovery searches the window for
//! messages never seen before. Reconciliation walks every tracked record
//! that's due, however old — which is what lets a PR merging a week after its
//! message still get its signal, and what lets the search window stay small.

use crate::adapters::store_json::{Outcome, Snapshot, Store, TrackedMessage, VERSION};
use crate::config::Config;
use crate::domain::model::{Message, MessageId, ReactionName, Reference, State};
use crate::domain::reconcile::{Plan, SkipReason, admit, reconcile};
use crate::domain::schedule;
use crate::domain::window::{SearchWindow, next_window};
use crate::ports::*;
use chrono::{DateTime, Utc};
use tracing::{debug, error, info, info_span, trace, warn};

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
    pub channel_name: String,
    pub posted_at: DateTime<Utc>,
    /// Who posted in Slack — often not who wrote the code.
    pub author: String,
    /// Each reference and whoever opened it.
    pub references: Vec<(String, String)>,
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

    /// Discovery itself failed, as distinct from individual records failing.
    /// The per-record counters stay at zero in this case, so a caller
    /// summarizing the run has to ask separately.
    pub fn discovery_failed(&self) -> bool {
        self.outcome == Some(Outcome::Failure)
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
        let started = std::time::Instant::now();
        let now = self.clock.now();
        // Field-free on purpose: span fields are formatted once, by whichever
        // layer initializes first, so a colored terminal layer would bake ANSI
        // escapes into the log file. Per-run detail goes on the events below.
        let _span = info_span!("run").entered();
        let mut snapshot = self.store.load()?;
        snapshot.version = VERSION;
        snapshot.run.last_run_started_at = Some(now);

        let mut report = Report::default();
        info!(
            dry_run = self.dry_run,
            tracked.before = snapshot.tracked.len(),
            last_outcome = ?snapshot.run.last_run_outcome,
            "run starting"
        );

        // Discovery. A failed scope must not advance its watermark: a gap in
        // discovery is the one thing no later run recovers from.
        let discovery_ok = self.discover(&mut snapshot, now, &mut report);

        self.name_authors(&mut snapshot);
        self.reconcile_due(&mut snapshot, now, &mut report);
        self.prune(&mut snapshot, now);

        if !discovery_ok {
            report.outcome = Some(Outcome::Failure);
        }

        report.tracked = snapshot.tracked.len();
        report.terminal = snapshot.tracked.values().filter(|t| t.terminal).count();
        snapshot.run.last_run_outcome = Some(report.outcome());

        if !self.dry_run {
            self.store.save(&snapshot)?;
        }

        info!(
            discovered = report.discovered,
            checked = report.checked,
            added = report.added.len(),
            skipped = report.skipped,
            failed = report.failed,
            tracked = report.tracked,
            terminal = report.terminal,
            outcome = ?report.outcome(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "run finished"
        );
        Ok(report)
    }

    /// Returns whether discovery itself succeeded.
    fn discover(&self, snapshot: &mut Snapshot, now: DateTime<Utc>, report: &mut Report) -> bool {
        let mut scopes = Vec::new();
        if self.config.slack.personal {
            match &self.config.slack.user_id {
                Some(user_id) => scopes.push(Scope::Personal(user_id.clone())),
                // Resolved before the run; if it isn't here, personal mode
                // can't be searched and silently skipping it would hide that.
                None => {
                    error!("personal mode is on but no user id was resolved");
                    return false;
                }
            }
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
            let key = scope.key();
            // Each scope remembers its own progress, so adding a channel gives
            // it a first-run window rather than inheriting "already covered"
            // from scopes that have been running for weeks.
            let computed = next_window(
                &self.config.window,
                snapshot.run.scopes.get(&key).copied(),
                now,
            );
            let window = SearchWindow {
                start: self.since.unwrap_or(computed.start),
                ..computed
            };
            if window.is_empty() {
                continue;
            }
            debug!(
                scope = %key,
                start = %window.start,
                hours = (window.end - window.start).num_minutes() as f64 / 60.0,
                first_run = !snapshot.run.scopes.contains_key(&key),
                "searching"
            );

            match self.source.discover(scope, &window) {
                Ok(found) => {
                    debug!(
                        scope = %key,
                        returned = found.messages.len(),
                        complete = found.complete,
                        "searched"
                    );
                    // Results come oldest-first, so a capped page is a prefix
                    // of the window. Advancing only to the newest message we
                    // saw leaves the remainder for the next run instead of
                    // stepping over it.
                    let covered_to = match found.complete {
                        true => window.end,
                        false => found
                            .messages
                            .iter()
                            .map(|m| m.posted_at)
                            .max()
                            .unwrap_or(window.start),
                    };
                    if !found.complete {
                        info!(
                            scope = %key,
                            covered_to = %covered_to,
                            "search filled a page; the rest of the window waits for the next run"
                        );
                    }
                    for message in found.messages {
                        if self.track(snapshot, message, now, report) {
                            report.discovered += 1;
                        }
                    }
                    // A --since that *narrowed* the window must not advance
                    // the mark, or a hand-run after a long sleep would
                    // silently skip the gap it didn't cover.
                    if window.start <= computed.start {
                        snapshot.run.scopes.insert(key, covered_to);
                        snapshot.run.last_successful_discovery_at = Some(covered_to);
                    } else {
                        info!(scope = %key, "watermark held: --since narrowed the window");
                    }
                }
                Err(error) => {
                    // Never fatal to the run — reconciliation of already
                    // tracked records is independent of discovery, and the
                    // other scopes are independent of this one.
                    error!(scope = %key, %error, "discovery failed");
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
        // Message text is the private part; it only appears at trace.
        trace!(message = %message.id, text = %message.text, "considering");
        match admit(&message, self.config) {
            Ok(references) => {
                debug!(
                    message = %message.id,
                    refs = references.len(),
                    posted_at = %message.posted_at,
                    "now tracking"
                );
                snapshot.tracked.insert(
                    key,
                    TrackedMessage {
                        id: message.id,
                        channel_name: message.channel_name,
                        author: message.author,
                        author_name: message.author_name,
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
            // Most messages have no PR link; that's not worth a log line.
            Err(SkipReason::NoReference) => false,
            Err(reason) => {
                debug!(message = %message.id, ?reason, "skipped");
                report.skipped += 1;
                false
            }
        }
    }

    /// Fill in real names for authors we only know by ID. Display-only, so a
    /// failure here is logged and shrugged off rather than failing the run.
    fn name_authors(&self, snapshot: &mut Snapshot) {
        // Search reports a username, not a name. Resolve anyone we haven't
        // looked up before, even if a username is already sitting there —
        // "dpepper" is a handle, and the point is to show a person.
        let mut unknown: Vec<String> = snapshot
            .tracked
            .values()
            .map(|t| t.author.clone())
            .filter(|id| !id.is_empty() && !snapshot.users.contains_key(id))
            .collect();
        unknown.sort();
        unknown.dedup();

        if !unknown.is_empty() {
            match self.source.user_names(&unknown) {
                Ok(found) => snapshot.users.extend(found),
                Err(error) => debug!(%error, "could not resolve author names"),
            }
        }
        for record in snapshot.tracked.values_mut() {
            if let Some(name) = snapshot.users.get(&record.author) {
                record.author_name = name.clone();
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
        debug!(due = ids.len(), "reconciling");
        let known = match self.source.reactions(&ids) {
            Ok(found) => Some(found),
            Err(error) => {
                // Not fatal: adds are idempotent. But without evidence we
                // can't claim ownership of what we add.
                warn!(%error, "could not read reactions; adds this run stay unclaimed");
                None
            }
        };

        for key in due {
            let Some(record) = snapshot.tracked.get(&key).cloned() else {
                continue;
            };
            report.checked += 1;

            let states = match self.provider.lookup(&record.references) {
                Ok(results) => results,
                Err(error) => {
                    error!(message = %record.id, %error, "status lookup failed");
                    report.failed += 1;
                    continue;
                }
            };
            // A reference nobody can read leaves the message alone rather
            // than letting its siblings speak for it.
            if let Some((reference, Err(why))) = states.iter().find(|(_, s)| s.is_err()) {
                warn!(
                    message = %record.id,
                    reference = %reference,
                    error = %why,
                    "reference unreadable; leaving the message alone"
                );
                report.failed += 1;
                continue;
            }
            // Authors are display-only, but come from the same lookup, so
            // they ride along rather than costing a second request.
            let resolved: Vec<(Reference, RefStatus)> = states
                .into_iter()
                .filter_map(|(r, s)| s.ok().map(|status| (r, status)))
                .collect();
            let states: Vec<State> = resolved.iter().map(|(_, s)| s.state).collect();

            let existing = known.as_ref().and_then(|m| m.get(&record.id).cloned());
            let plan = reconcile(&states, existing.as_deref(), self.config);

            // The line that answers "why didn't it react to my message?".
            debug!(
                message = %record.id,
                refs = %record.references.iter().map(ToString::to_string).collect::<Vec<_>>().join(","),
                states = ?states,
                aggregate = ?plan.aggregate,
                existing = ?existing.as_ref().map(|r| r.iter().map(ToString::to_string).collect::<Vec<_>>()),
                desired = ?plan.desired.as_ref().map(ToString::to_string),
                action = ?plan.add.as_ref().map(ToString::to_string),
                terminal = plan.terminal,
                "reconciled"
            );

            report.details.push(Detail {
                id: record.id.clone(),
                permalink: record.permalink.clone(),
                channel_name: record.channel_name.clone(),
                posted_at: record.posted_at,
                author: record.author_name.clone(),
                references: resolved
                    .iter()
                    .map(|(r, s)| (r.to_string(), s.author.clone()))
                    .collect(),
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
                        info!(
                            message = %id,
                            emoji = %emoji,
                            state = ?plan.aggregate,
                            claimed = plan.ledger,
                            "reacted"
                        );
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
                        info!(message = %id, "message is gone; stopping tracking it");
                        snapshot.tracked.remove(key);
                        return;
                    }
                    Err(error @ SinkError::InvalidEmoji(_)) => {
                        // Retrying won't help — the workspace needs the emoji.
                        error!(message = %id, %error, "check the emoji names in your config");
                        report.failed += 1;
                    }
                    Err(error) => {
                        error!(message = %id, %error, "could not react");
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
