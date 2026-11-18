//! Run state on disk: one JSON file, written atomically.
//!
//! Not a database. Steady state is a few hundred live records, the access
//! pattern is load-all/save-all, and being able to read and hand-edit the file
//! at 2am is worth more than query support. The `Store` trait is the seam if
//! that stops being true.

use crate::adapters::lock::FileLock;
use crate::domain::model::{MessageId, ReactionName};
use crate::domain::model::{Reference, State};
use crate::ports::StoreError;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Bumped only when the on-disk shape changes incompatibly.
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    #[serde(default)]
    pub run: RunState,
    /// Tracked messages, keyed by `channel:ts`.
    #[serde(default)]
    pub tracked: BTreeMap<String, TrackedMessage>,
    /// Channel handle to ID. IDs never change, and resolving one costs a
    /// bridge call, so this is worth carrying between runs.
    #[serde(default)]
    pub channels: BTreeMap<String, String>,
    /// User ID to real name. Slack search only reports usernames, and a
    /// profile call per person is worth making exactly once.
    #[serde(default)]
    pub users: BTreeMap<String, String>,
    /// Whose credentials these are, resolved once rather than configured.
    #[serde(default)]
    pub user_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunState {
    /// How far each scope has been searched, keyed by `Scope::key`.
    ///
    /// Per scope rather than one watermark, because they aren't
    /// interchangeable: a channel added today has never been searched, and a
    /// shared watermark would tell it the last few minutes were already
    /// covered — so it would find nothing until someone posted again.
    #[serde(default)]
    pub scopes: BTreeMap<String, DateTime<Utc>>,
    /// Most recent successful discovery in any scope. Display only.
    #[serde(default)]
    pub last_successful_discovery_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_run_started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_run_outcome: Option<Outcome>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Success,
    /// Discovery worked, but at least one record failed. Retried next run.
    Partial,
    /// Discovery itself failed; the watermark must not advance.
    Failure,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackedMessage {
    pub id: MessageId,
    #[serde(default)]
    pub permalink: String,
    /// Display only, captured at discovery so printing a record later needs
    /// no extra lookup.
    #[serde(default)]
    pub channel_name: String,
    /// Slack user ID of whoever posted.
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub author_name: String,
    pub references: Vec<Reference>,
    #[serde(default)]
    pub last_state: Option<State>,
    /// Reference display string to pull-request title, refreshed on each
    /// check. Kept so a record can be listed without looking it up again —
    /// most tracked records aren't due on any given run.
    #[serde(default)]
    pub titles: BTreeMap<String, String>,
    /// What Inkling itself added — never what a human did. Only written when
    /// evidence showed the emoji absent beforehand.
    #[serde(default)]
    pub applied: Vec<ReactionName>,
    pub posted_at: DateTime<Utc>,
    pub first_seen: DateTime<Utc>,
    pub last_checked: Option<DateTime<Utc>>,
    #[serde(default)]
    pub terminal: bool,
}

impl TrackedMessage {
    pub fn key(&self) -> String {
        self.id.to_string()
    }
}

pub trait Store {
    fn load(&self) -> Result<Snapshot, StoreError>;
    fn save(&self, snapshot: &Snapshot) -> Result<(), StoreError>;
}

pub struct JsonStore {
    path: PathBuf,
}

impl JsonStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// `$XDG_STATE_HOME/inkling/state.json`, or the platform equivalent.
    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("XDG_STATE_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
            })
            .unwrap_or_else(std::env::temp_dir);
        base.join("inkling").join("state.json")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Change a cached value under the run lock: load, change, save. Nothing
    /// slow belongs inside `change` — do the lookup first, then this.
    ///
    /// Doesn't wait for a run, which holds the lock for minutes: a cache that
    /// can't be written costs a lookup next time, nothing more.
    pub fn update<T>(&self, change: impl FnOnce(&mut Snapshot) -> T) -> Result<T, StoreError> {
        let _lock = FileLock::try_acquire(&FileLock::path_for(&self.path))
            .map_err(|e| StoreError::Write(e.to_string()))?
            .ok_or(StoreError::Busy { waited_ms: 0 })?;
        let mut snapshot = self.load()?;
        let outcome = change(&mut snapshot);
        self.save(&snapshot)?;
        Ok(outcome)
    }

    /// Move a file we can't read out of the way rather than overwriting it —
    /// it's the only copy of the ownership ledger, and worth keeping around
    /// for a look even when it's unparseable.
    fn quarantine(&self) -> Option<PathBuf> {
        for n in 0..100 {
            let candidate = self.path.with_extension(format!("corrupt-{n}"));
            if !candidate.exists() {
                std::fs::rename(&self.path, &candidate).ok()?;
                return Some(candidate);
            }
        }
        None
    }
}

impl Store for JsonStore {
    fn load(&self) -> Result<Snapshot, StoreError> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            // A missing file is a first run, not a failure.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Snapshot {
                    version: VERSION,
                    ..Snapshot::default()
                });
            }
            Err(e) => return Err(StoreError::Read(e.to_string())),
        };

        match serde_json::from_str::<Snapshot>(&raw) {
            Ok(snapshot) if snapshot.version > VERSION => Err(StoreError::FutureVersion {
                found: snapshot.version,
            }),
            Ok(snapshot) => Ok(snapshot),
            Err(_) => {
                let moved = self.quarantine();
                tracing::error!(
                    backup = ?moved,
                    "state file was unreadable; starting fresh (ownership ledger lost, \
                     so nothing already applied will be claimed)"
                );
                Ok(Snapshot {
                    version: VERSION,
                    ..Snapshot::default()
                })
            }
        }
    }

    fn save(&self, snapshot: &Snapshot) -> Result<(), StoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StoreError::Write(e.to_string()))?;
        }
        let json =
            serde_json::to_string_pretty(snapshot).map_err(|e| StoreError::Write(e.to_string()))?;

        // Temp file plus rename, so a crash mid-write can't leave a truncated
        // state file. The watermark and the tracked set commit together.
        let temp = self.path.with_extension("tmp");
        std::fs::write(&temp, json).map_err(|e| StoreError::Write(e.to_string()))?;
        std::fs::rename(&temp, &self.path).map_err(|e| StoreError::Write(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("inkling-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn file(&self) -> PathBuf {
            self.0.join("state.json")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tracked() -> TrackedMessage {
        TrackedMessage {
            id: MessageId::new("C1", "1786596050.245879"),
            permalink: "https://example.slack.com/p1".into(),
            channel_name: "inkling".into(),
            author: "U1".into(),
            author_name: "Daniel Pepper".into(),
            references: vec![Reference::GitHubPullRequest {
                host: "github.com".into(),
                owner: "dpep".into(),
                repo: "gqls".into(),
                number: 1,
            }],
            last_state: Some(State::Approved),
            titles: BTreeMap::new(),
            applied: vec![ReactionName::new("white_check_mark")],
            posted_at: "2026-08-12T21:21:16Z".parse().unwrap(),
            first_seen: "2026-08-12T21:30:00Z".parse().unwrap(),
            last_checked: Some("2026-08-12T21:30:00Z".parse().unwrap()),
            terminal: false,
        }
    }

    #[test]
    fn a_missing_file_reads_as_a_first_run() {
        let dir = TempDir::new("missing");
        let snapshot = JsonStore::new(dir.file()).load().unwrap();
        assert_eq!(snapshot.version, VERSION);
        assert!(snapshot.tracked.is_empty());
        assert!(snapshot.run.last_successful_discovery_at.is_none());
    }

    #[test]
    fn a_snapshot_round_trips_including_the_ledger() {
        let dir = TempDir::new("roundtrip");
        let store = JsonStore::new(dir.file());

        let mut snapshot = Snapshot {
            version: VERSION,
            ..Snapshot::default()
        };
        let message = tracked();
        snapshot.tracked.insert(message.key(), message);
        snapshot.channels.insert("inkling".into(), "C1".into());
        snapshot.run.last_successful_discovery_at = Some("2026-08-12T21:30:00Z".parse().unwrap());
        snapshot.run.last_run_outcome = Some(Outcome::Success);

        store.save(&snapshot).unwrap();
        assert_eq!(store.load().unwrap(), snapshot);
    }

    #[test]
    fn a_corrupt_file_is_moved_aside_rather_than_overwritten() {
        // It holds the only copy of the ownership ledger.
        let dir = TempDir::new("corrupt");
        let store = JsonStore::new(dir.file());
        std::fs::write(dir.file(), "{not json at all").unwrap();

        let snapshot = store.load().unwrap();
        assert!(snapshot.tracked.is_empty());
        assert!(dir.file().with_extension("corrupt-0").exists());
    }

    #[test]
    fn a_newer_version_refuses_rather_than_clobbering() {
        let dir = TempDir::new("future");
        let store = JsonStore::new(dir.file());
        std::fs::write(dir.file(), r#"{"version": 999, "tracked": {}}"#).unwrap();

        assert!(matches!(
            store.load(),
            Err(StoreError::FutureVersion { found: 999 })
        ));
    }

    #[test]
    fn saving_creates_the_directory() {
        let dir = TempDir::new("nested");
        let path = dir.0.join("deep").join("state.json");
        let store = JsonStore::new(&path);
        store.save(&Snapshot::default()).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn a_save_leaves_no_temp_file_behind() {
        let dir = TempDir::new("atomic");
        let store = JsonStore::new(dir.file());
        store.save(&Snapshot::default()).unwrap();
        assert!(!dir.file().with_extension("tmp").exists());
    }

    #[test]
    fn a_cache_write_keeps_what_another_writer_saved_meanwhile() {
        // Found by the hunt: `watch slack:#new` loaded the state, spent a
        // bridge call resolving the channel, and saved its stale copy over
        // whatever `inkling run` had written in between — ledger included.
        let dir = TempDir::new("merge");
        let store = JsonStore::new(dir.file());
        let stale = store.load().unwrap();
        let mut ran = stale.clone();
        ran.tracked.insert(tracked().key(), tracked());
        store.save(&ran).unwrap();

        store
            .update(|s| s.channels.insert("general".into(), "C1".into()))
            .unwrap();

        let now = store.load().unwrap();
        assert_eq!(now.tracked.len(), 1, "run's write survived");
        assert_eq!(now.channels["general"], "C1");
    }

    #[test]
    fn a_cache_write_waits_out_no_run() {
        let dir = TempDir::new("held");
        let store = JsonStore::new(dir.file());
        let _run = crate::adapters::lock::FileLock::try_acquire(
            &crate::adapters::lock::FileLock::path_for(store.path()),
        )
        .unwrap()
        .unwrap();
        assert!(matches!(
            store.update(|s| s.user_id = Some("U1".into())),
            Err(StoreError::Busy { .. })
        ));
    }

    #[test]
    fn a_cache_write_never_lands_on_a_file_it_could_not_read() {
        let dir = TempDir::new("update-future");
        std::fs::write(dir.file(), r#"{"version": 999}"#).unwrap();
        let store = JsonStore::new(dir.file());
        assert!(store.update(|s| s.user_id = Some("U1".into())).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.file()).unwrap(),
            r#"{"version": 999}"#
        );
    }

    #[test]
    fn the_default_path_lives_under_the_state_directory() {
        let path = JsonStore::default_path();
        assert!(path.ends_with("inkling/state.json"), "{path:?}");
    }
}
