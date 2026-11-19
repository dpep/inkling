//! What Slack lookups have taught us, on disk: one small JSON file, written
//! atomically, holding answers that cost a bridge call and never change.
//!
//! A file written by 0.3.0 or earlier also held the Slack reactions' ledger;
//! serde skips those fields on load, and the next cache write drops them.

use crate::adapters::lock::FileLock;
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
    /// Channel name to ID. IDs never change, and resolving one costs a
    /// bridge call, so this is worth carrying between processes.
    #[serde(default)]
    pub channels: BTreeMap<String, String>,
    /// Whose credentials these are, resolved once rather than configured.
    #[serde(default)]
    pub user_id: Option<String>,
    /// When finding that out last failed: each try is paid, and every CLI
    /// process would otherwise pay again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id_failed_at: Option<DateTime<Utc>>,
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

    /// Change a cached value under the lock: load, change, save. Nothing
    /// slow belongs inside `change` — do the lookup first, then this.
    ///
    /// Doesn't wait for the lock: a cache that can't be written costs a
    /// lookup next time, nothing more.
    pub fn update<T>(&self, change: impl FnOnce(&mut Snapshot) -> T) -> Result<T, StoreError> {
        let _lock = FileLock::try_acquire(&FileLock::path_for(&self.path))
            .map_err(|e| StoreError::Write(e.to_string()))?
            .ok_or(StoreError::Busy { waited_ms: 0 })?;
        let mut snapshot = self.load()?;
        let outcome = change(&mut snapshot);
        self.save(&snapshot)?;
        Ok(outcome)
    }

    /// Move a file we can't read out of the way rather than overwriting it,
    /// so it's still there for a look.
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

    pub fn load(&self) -> Result<Snapshot, StoreError> {
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
                tracing::error!(backup = ?moved, "state file was unreadable; starting fresh");
                Ok(Snapshot {
                    version: VERSION,
                    ..Snapshot::default()
                })
            }
        }
    }

    pub fn save(&self, snapshot: &Snapshot) -> Result<(), StoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StoreError::Write(e.to_string()))?;
        }
        let json =
            serde_json::to_string_pretty(snapshot).map_err(|e| StoreError::Write(e.to_string()))?;

        // Temp file plus rename, so a crash mid-write can't leave a truncated
        // state file.
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

    #[test]
    fn a_missing_file_reads_as_a_first_run() {
        let dir = TempDir::new("missing");
        let snapshot = JsonStore::new(dir.file()).load().unwrap();
        assert_eq!(snapshot.version, VERSION);
        assert!(snapshot.channels.is_empty());
        assert!(snapshot.user_id.is_none());
    }

    #[test]
    fn a_snapshot_round_trips() {
        let dir = TempDir::new("roundtrip");
        let store = JsonStore::new(dir.file());

        let mut snapshot = Snapshot {
            version: VERSION,
            user_id: Some("U1".into()),
            ..Snapshot::default()
        };
        snapshot.channels.insert("inkling".into(), "C1".into());

        store.save(&snapshot).unwrap();
        assert_eq!(store.load().unwrap(), snapshot);
    }

    #[test]
    fn a_file_from_before_the_reactions_were_removed_keeps_its_caches() {
        // 0.3.0 kept the reactions' ledger here too. Its caches still count;
        // the ledger is skipped, and gone after the next write.
        let dir = TempDir::new("ledger");
        std::fs::write(
            dir.file(),
            r#"{"version": 1,
                "run": {"scopes": {"personal": "2026-08-12T21:30:00Z"}, "last_run_outcome": "success"},
                "tracked": {"C1:1786596050.245879": {"id": {"channel": "C1", "ts": "1786596050.245879"},
                    "references": [], "applied": ["rocket"], "posted_at": "2026-08-12T21:21:16Z",
                    "first_seen": "2026-08-12T21:30:00Z", "last_checked": null}},
                "channels": {"inkling": "C1"},
                "users": {"U1": "Someone"},
                "user_id": "U1"}"#,
        )
        .unwrap();
        let store = JsonStore::new(dir.file());

        let snapshot = store.load().unwrap();
        assert_eq!(snapshot.user_id.as_deref(), Some("U1"));
        assert_eq!(snapshot.channels["inkling"], "C1");
        assert!(!dir.file().with_extension("corrupt-0").exists());

        store.update(|_| ()).unwrap();
        let written = std::fs::read_to_string(dir.file()).unwrap();
        assert!(!written.contains("tracked"), "{written}");
    }

    #[test]
    fn a_corrupt_file_is_moved_aside_rather_than_overwritten() {
        let dir = TempDir::new("corrupt");
        let store = JsonStore::new(dir.file());
        std::fs::write(dir.file(), "{not json at all").unwrap();

        let snapshot = store.load().unwrap();
        assert!(snapshot.channels.is_empty());
        assert!(dir.file().with_extension("corrupt-0").exists());
    }

    #[test]
    fn a_newer_version_refuses_rather_than_clobbering() {
        let dir = TempDir::new("future");
        let store = JsonStore::new(dir.file());
        std::fs::write(dir.file(), r#"{"version": 999, "channels": {}}"#).unwrap();

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
        // Regression: `watch slack:#new` loaded the state, spent a
        // bridge call resolving the channel, and saved its stale copy over
        // whatever another process had written in between.
        let dir = TempDir::new("merge");
        let store = JsonStore::new(dir.file());
        let stale = store.load().unwrap();
        let mut other = stale.clone();
        other.user_id = Some("U1".into());
        store.save(&other).unwrap();

        store
            .update(|s| s.channels.insert("general".into(), "C1".into()))
            .unwrap();

        let now = store.load().unwrap();
        assert_eq!(
            now.user_id.as_deref(),
            Some("U1"),
            "the other write survived"
        );
        assert_eq!(now.channels["general"], "C1");
    }

    #[test]
    fn a_cache_write_waits_out_no_lock() {
        let dir = TempDir::new("held");
        let store = JsonStore::new(dir.file());
        let _held = crate::adapters::lock::FileLock::try_acquire(
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
