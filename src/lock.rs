//! Exclusive locks for the `~/.meta/` stores.
//!
//! The lock is an OS file lock (`flock` on Unix, `LockFileEx` on Windows) held
//! on an open lock file. The OS releases it when the holder exits, crashes or
//! is killed, so there is no stale state to detect or recover: a leftover lock
//! file, empty or holding a PID from an older meta, is just a file and gets
//! locked again. The file is never deleted, so two processes racing can never
//! end up holding locks on two different files at the same path.
//!
//! On Unix the lock's directory must belong to the current user and must not
//! be writable by group or others; anyone else who could write there could
//! replace the lock file or the store beside it.

use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

/// RAII guard that holds the lock until dropped.
pub struct LockGuard {
    _file: File,
    path: PathBuf,
}

impl LockGuard {
    /// Get the path of the lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Acquire an exclusive lock on `lock_path`, creating the file and its
/// directory if needed.
///
/// Waits up to `timeout` while another process holds the lock, then fails.
pub fn acquire(lock_path: &Path, timeout: Duration) -> Result<LockGuard> {
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create lock directory: {}", parent.display()))?;
        crate::data_dir::check_private_dir(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .with_context(|| format!("Failed to open lock file: {}", lock_path.display()))?;

    let deadline = Instant::now() + timeout;
    loop {
        match file.try_lock() {
            Ok(()) => {
                return Ok(LockGuard {
                    _file: file,
                    path: lock_path.to_path_buf(),
                })
            }
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(TryLockError::WouldBlock) => anyhow::bail!(
                "Timed out after {:?} waiting for the lock at {}: another meta process holds it",
                timeout,
                lock_path.display()
            ),
            Err(TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("Failed to lock {}", lock_path.display()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAIT: Duration = Duration::from_millis(200);

    #[test]
    fn test_acquire_and_release() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join("test.lock");

        let guard = acquire(&lock_path, WAIT).unwrap();
        assert_eq!(guard.path(), lock_path);
        drop(guard);

        // Released on drop: the next acquire succeeds at once
        acquire(&lock_path, Duration::ZERO).unwrap();
    }

    #[test]
    fn test_acquire_fails_when_locked() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join("test.lock");

        let _guard = acquire(&lock_path, WAIT).unwrap();
        let err = acquire(&lock_path, WAIT).err().unwrap().to_string();
        assert!(err.contains("another meta process holds it"), "{err}");
    }

    #[test]
    fn test_leftover_lock_files_do_not_block() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join("stale.lock");

        // Empty (crash between create and write), garbage, a dead PID, and a
        // live but unrelated PID (our own, standing in for PID reuse)
        let own_pid = format!("{}\n", std::process::id());
        for content in ["", "not a pid", "999999999\n", own_pid.as_str()] {
            fs::write(&lock_path, content).unwrap();
            acquire(&lock_path, Duration::ZERO).unwrap();
        }
    }

    #[test]
    fn test_lock_excludes_other_processes() {
        // The lock belongs to the open file, so a second open (as another
        // process would make) sees it held.
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join("x.lock");
        let _guard = acquire(&lock_path, WAIT).unwrap();

        let other = File::options().write(true).open(&lock_path).unwrap();
        assert!(matches!(other.try_lock(), Err(TryLockError::WouldBlock)));
    }
}
