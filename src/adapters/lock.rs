//! An advisory lock file, so two processes don't clobber one JSON file.
//!
//! `flock` rather than a crate: the only callers are on macOS and Linux,
//! `libc` is already a direct dependency, and the whole mechanism is a dozen
//! lines. The kernel releases the lock when the file closes — including on
//! crash or kill — so a stale lock can't outlive the process holding it,
//! which is the property a hand-rolled lockfile would get wrong.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("opening lock file {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("locking {path}: {source}")]
    Lock {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Held for as long as the guard lives.
#[derive(Debug)]
pub struct FileLock {
    // Never read: the lock lives on the open descriptor, and dropping the
    // File closes it, which is what releases the lock.
    _file: File,
}

impl FileLock {
    /// The lock file that guards `path` — a sibling, so the guarded file can
    /// still be replaced by atomic rename without disturbing the lock.
    pub fn path_for(path: &Path) -> PathBuf {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".lock");
        path.with_file_name(name)
    }

    /// Take the lock, or return `None` if someone else holds it. Never
    /// blocks — a caller who can't proceed usually has something better to do
    /// than queue.
    pub fn try_acquire(path: &Path) -> Result<Option<Self>, LockError> {
        let file = open(path)?;
        match flock(&file, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(source) => Err(LockError::Lock {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Take the lock, waiting as long as it takes.
    ///
    /// Used for the short load-modify-save critical sections, where the hold
    /// time is a few milliseconds of JSON and blocking is simpler than any
    /// retry loop that would replace it.
    pub fn acquire(path: &Path) -> Result<Self, LockError> {
        let file = open(path)?;
        flock(&file, libc::LOCK_EX).map_err(|source| LockError::Lock {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Self { _file: file })
    }
}

fn open(path: &Path) -> Result<File, LockError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| LockError::Open {
            path: path.to_path_buf(),
            source,
        })?;
    }
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|source| LockError::Open {
            path: path.to_path_buf(),
            source,
        })
}

fn flock(file: &File, operation: i32) -> io::Result<()> {
    match unsafe { libc::flock(file.as_raw_fd(), operation) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("inkling-lock-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("state.json")
    }

    #[test]
    fn the_lock_file_sits_beside_what_it_guards() {
        let path = FileLock::path_for(Path::new("/var/state/inkling/state.json"));
        assert_eq!(path, Path::new("/var/state/inkling/state.json.lock"));
    }

    #[test]
    fn a_second_holder_is_refused_while_the_first_lives() {
        let path = FileLock::path_for(&temp("contended"));
        let first = FileLock::try_acquire(&path).unwrap();
        assert!(first.is_some(), "the first caller takes it");
        assert!(
            FileLock::try_acquire(&path).unwrap().is_none(),
            "the second is refused rather than made to wait"
        );
    }

    #[test]
    fn dropping_the_guard_releases_it() {
        // The whole safety story rests on this: a run that ends — cleanly or
        // not — must not leave the next one locked out.
        let path = FileLock::path_for(&temp("released"));
        drop(FileLock::try_acquire(&path).unwrap());
        assert!(FileLock::try_acquire(&path).unwrap().is_some());
    }

    #[test]
    fn the_lock_file_is_created_along_with_its_directory() {
        let path = FileLock::path_for(&temp("nested").parent().unwrap().join("deep/state.json"));
        let _held = FileLock::try_acquire(&path).unwrap().unwrap();
        assert!(path.exists());
    }
}
