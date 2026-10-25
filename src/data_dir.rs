//! `~/.meta/` directory management.
//!
//! Provides functions to locate, create, and manage namespaced data files
//! within the meta data directory. Any crate/plugin can use this module to
//! store data at `~/.meta/<namespace>.json` or `~/.meta/<namespace>/`.
//!
//! The directory holds per-user state (stores, locks, plugins), so meta creates
//! it private to the user and refuses one that another user owns or can write
//! to.
//!
//! Use `meta_core::meta_dir()` to get the directory path directly.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Ensure the meta data directory exists, creating it if needed.
/// Returns the path to the directory.
pub fn ensure_meta_dir() -> Result<PathBuf> {
    let dir = crate::meta_dir()?;
    create_private_dir(&dir)?;
    Ok(dir)
}

/// The meta data directory, checked with [`check_private_dir`] when it exists,
/// so nothing is read from one that another user could have written to.
pub fn private_meta_dir() -> Result<PathBuf> {
    let dir = crate::meta_dir()?;
    if dir.exists() {
        check_private_dir(&dir)?;
    }
    Ok(dir)
}

/// Get the path for a namespaced data file: `~/.meta/<namespace>.json`.
///
/// The file may or may not exist. Use `store::read` to read with a default,
/// or check existence manually.
pub fn data_file(namespace: &str) -> Result<PathBuf> {
    Ok(private_meta_dir()?.join(format!("{namespace}.json")))
}

/// Get the path for a namespaced subdirectory: `~/.meta/<namespace>/`.
/// Creates the directory if it doesn't exist.
pub fn data_subdir(namespace: &str) -> Result<PathBuf> {
    let dir = ensure_meta_dir()?.join(namespace);
    create_private_dir(&dir)?;
    Ok(dir)
}

/// Create `dir` (mode 0700 on Unix) if missing, then check it is private.
fn create_private_dir(dir: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir).with_context(|| {
        format!(
            "Failed to create meta data directory at {}",
            crate::display::escape_path(dir)
        )
    })?;
    check_private_dir(dir)
}

/// Refuse a directory that another user owns or can write to: they could
/// replace the stores, locks or plugins in it. The same goes for every
/// directory above it (after following symlinks), since another user who can
/// rename entries there could swap the whole directory: each must belong to
/// you or root and be writable by no one else, unless it is sticky (`/tmp`).
#[cfg(unix)]
pub fn check_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let shown = crate::display::escape_path(dir);
    let real = dir
        .canonicalize()
        .with_context(|| format!("Failed to resolve {shown}"))?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    let meta = std::fs::metadata(&real).with_context(|| format!("Failed to stat {shown}"))?;
    if meta.uid() != uid {
        anyhow::bail!(
            "Refusing to use {shown}: it is owned by uid {}, not by you (uid {uid})",
            meta.uid()
        );
    }
    if meta.mode() & 0o022 != 0 {
        anyhow::bail!(
            "Refusing to use {shown}: other users can write to it (fix with: chmod go-w '{shown}')"
        );
    }
    for ancestor in real.ancestors().skip(1) {
        let meta = std::fs::metadata(ancestor)
            .with_context(|| format!("Failed to stat {}", crate::display::escape_path(ancestor)))?;
        let sticky = meta.mode() & 0o1000 != 0;
        if (meta.uid() != uid && meta.uid() != 0) || (meta.mode() & 0o022 != 0 && !sticky) {
            anyhow::bail!(
                "Refusing to use {shown}: another user could replace it through {}",
                crate::display::escape_path(ancestor)
            );
        }
    }
    Ok(())
}

/// Refuse a directory that another user owns or can write to.
#[cfg(not(unix))]
pub fn check_private_dir(_dir: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_data_file_path() {
        std::env::set_var("META_DATA_DIR", "/tmp/test-meta");
        let path = data_file("worktree").unwrap();
        assert_eq!(path, PathBuf::from("/tmp/test-meta/worktree.json"));
        std::env::remove_var("META_DATA_DIR");
    }

    #[test]
    fn test_ensure_meta_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("META_DATA_DIR", tmp.path().join("meta-test"));
        let result = ensure_meta_dir().unwrap();
        assert!(result.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&result).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        std::env::remove_var("META_DATA_DIR");
    }

    #[cfg(unix)]
    #[test]
    fn test_refuses_private_dir_under_shared_parent() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let shared = tmp.path().join("shared");
        let dir = shared.join("mine");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        let err = check_private_dir(&dir).unwrap_err().to_string();
        assert!(err.contains("another user could replace it"), "{err}");

        // A sticky parent, like /tmp, keeps others from renaming it
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
        check_private_dir(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn test_refuses_dir_others_can_write() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("shared");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let err = check_private_dir(&dir).unwrap_err().to_string();
        assert!(err.contains("other users can write to it"), "{err}");
    }
}
