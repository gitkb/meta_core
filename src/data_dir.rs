//! `~/.meta/` directory management.
//!
//! Provides functions to locate, create, and manage namespaced data files
//! within the meta data directory. Any crate/plugin can use this module to
//! store data at `~/.meta/<namespace>.json` or `~/.meta/<namespace>/`.
//!
//! The directory holds per-user state (stores, plugins), so meta creates
//! it, and its subdirectories, private to the user (0700 on Unix).
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

/// Get the path for a namespaced data file: `~/.meta/<namespace>.json`.
///
/// The file may or may not exist. Use `store::read` to read with a default,
/// or check existence manually.
pub fn data_file(namespace: &str) -> Result<PathBuf> {
    Ok(crate::meta_dir()?.join(format!("{namespace}.json")))
}

/// Get the path for a namespaced subdirectory: `~/.meta/<namespace>/`.
/// Creates the directory if it doesn't exist.
pub fn data_subdir(namespace: &str) -> Result<PathBuf> {
    let dir = ensure_meta_dir()?.join(namespace);
    create_private_dir(&dir)?;
    Ok(dir)
}

/// Create `dir` and any missing parents (mode 0700 on Unix).
pub fn create_private_dir(dir: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir).with_context(|| {
        format!(
            "Failed to create meta data directory at {}",
            crate::display::escape_path(dir)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_data_file_path() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("META_DATA_DIR", tmp.path());
        let path = data_file("worktree").unwrap();
        assert_eq!(path, tmp.path().join("worktree.json"));
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
}
