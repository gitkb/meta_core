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
/// replace the stores, locks or plugins in it.
///
/// The same goes for every directory the path passes through, as written and
/// through each symlink it follows, since another user who can rename entries
/// in one of them could swap what the path leads to. Each such directory must
/// belong to you or root and be writable by no one else, unless it is sticky
/// like `/tmp` (then only an entry's owner can rename it).
#[cfg(unix)]
pub fn check_private_dir(dir: &Path) -> Result<()> {
    use std::ffi::OsString;
    use std::os::unix::fs::MetadataExt;
    use std::path::Component;

    let shown = crate::display::escape_path(dir);
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    let stat = |path: &Path| {
        std::fs::symlink_metadata(path).with_context(|| {
            format!(
                "Failed to check {shown}: cannot stat {}",
                crate::display::escape_path(path)
            )
        })
    };

    // Resolve the path one component at a time, checking the directory that
    // holds each entry before following it.
    let absolute = std::env::current_dir()
        .context("Failed to read the current directory")?
        .join(dir);
    let mut pending: Vec<OsString> = Vec::new();
    let push_rev = |pending: &mut Vec<OsString>, path: &Path| {
        for c in path.components().rev() {
            match c {
                Component::Normal(n) => pending.push(n.to_os_string()),
                Component::ParentDir => pending.push(OsString::from("..")),
                Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
            }
        }
    };
    push_rev(&mut pending, &absolute);
    let mut current = PathBuf::from("/");
    let mut hops = 0;
    while let Some(name) = pending.pop() {
        if name == ".." {
            current.pop();
            continue;
        }
        let holder = stat(&current)?;
        let sticky = holder.mode() & 0o1000 != 0;
        if (holder.uid() != uid && holder.uid() != 0) || (holder.mode() & 0o022 != 0 && !sticky) {
            anyhow::bail!(
                "Refusing to use {shown}: another user could replace it through {}",
                crate::display::escape_path(&current)
            );
        }
        let next = current.join(&name);
        if stat(&next)?.file_type().is_symlink() {
            hops += 1;
            anyhow::ensure!(hops <= 40, "Failed to check {shown}: too many symlinks");
            let target = std::fs::read_link(&next)
                .with_context(|| format!("Failed to check {shown}: cannot read a symlink"))?;
            if target.is_absolute() {
                current = PathBuf::from("/");
            }
            push_rev(&mut pending, &target);
        } else {
            current = next;
        }
    }

    let meta = stat(&current)?;
    if !meta.is_dir() || meta.uid() != uid {
        anyhow::bail!("Refusing to use {shown}: it is not a directory owned by you (uid {uid})");
    }
    if meta.mode() & 0o022 != 0 {
        anyhow::bail!(
            "Refusing to use {shown}: other users can write to it (fix with: chmod go-w '{shown}')"
        );
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

        // A link to it from a shared dir is refused too: the link can be swapped
        let link_home = tmp.path().join("links");
        std::fs::create_dir(&link_home).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
        check_private_dir(&dir).unwrap();
        std::os::unix::fs::symlink(&dir, link_home.join("meta")).unwrap();
        check_private_dir(&link_home.join("meta")).unwrap();
        std::fs::set_permissions(&link_home, std::fs::Permissions::from_mode(0o777)).unwrap();
        let err = check_private_dir(&link_home.join("meta"))
            .unwrap_err()
            .to_string();
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
