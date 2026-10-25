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
//! replace the lock file or the store beside it. The lock file itself is
//! 0600, since anyone who can open it can hold the lock.

use crate::display::escape_path;
use anyhow::{Context, Result};
use std::fs::{File, OpenOptions, TryLockError};
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
        crate::data_dir::create_private_dir(parent)?;
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    // Readable by no one else: anyone who can open the file can hold the
    // lock and block every store write
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options
        .open(lock_path)
        .with_context(|| format!("Failed to open lock file: {}", escape_path(lock_path)))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // a lock file left by an older meta may still be 0644
        let mode = file
            .metadata()
            .with_context(|| format!("Failed to stat {}", escape_path(lock_path)))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("Failed to restrict {}", escape_path(lock_path)))?;
        }
    }
    // An access list can still let others open it: refuse, rather than
    // wait on a lock someone else may hold forever
    #[cfg(target_os = "macos")]
    if crate::data_dir::acl_allows_others(lock_path) {
        anyhow::bail!(
            "Refusing to lock {}: its access control list lets others open it \
             (remove with: chmod -N)",
            escape_path(lock_path)
        );
    }
    #[cfg(windows)]
    {
        use crate::win_acl::{current_user_sid, untrusted_access, READ_RIGHTS};
        if let Some(why) = untrusted_access(lock_path, &current_user_sid()?, READ_RIGHTS, "open")? {
            anyhow::bail!(
                "Refusing to lock {}: {why}, so another account could hold the lock",
                escape_path(lock_path)
            );
        }
    }

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
                escape_path(lock_path)
            ),
            Err(TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("Failed to lock {}", escape_path(lock_path)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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

    #[cfg(unix)]
    #[test]
    fn test_lock_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join("x.lock");
        // An older meta's leftover lock file, readable by everyone
        fs::write(&lock_path, "123\n").unwrap();
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o644)).unwrap();
        drop(acquire(&lock_path, WAIT).unwrap());
        let mode = fs::metadata(&lock_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
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
