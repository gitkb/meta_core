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
pub(crate) fn create_private_dir(dir: &Path) -> Result<()> {
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

/// Refuse a file that meta will run, such as a plugin, if another user could
/// change or replace it: the directory it really lives in (after symlinks)
/// must pass [`check_private_dir`], and the file itself must belong to you
/// or root and be writable by no one else.
pub fn check_trusted_file(path: &Path) -> Result<()> {
    let shown = crate::display::escape_path(path);
    let real = path
        .canonicalize()
        .with_context(|| format!("Failed to resolve {shown}"))?;
    if let Some(parent) = real.parent() {
        check_private_dir(parent)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(&real).with_context(|| format!("Failed to stat {shown}"))?;
        // SAFETY: geteuid has no preconditions and cannot fail.
        let uid = unsafe { libc::geteuid() };
        if !owned_by_you_or_root(&meta, uid) || meta.mode() & 0o022 != 0 || acl_allows_others(&real)
        {
            anyhow::bail!(
                "Refusing to use {shown}: another user owns it or can change it \
                 (fix with: chmod go-w '{shown}')"
            );
        }
    }
    #[cfg(windows)]
    {
        use crate::win_acl::{current_user_sid, untrusted_access, DIR_RIGHTS};
        let user = current_user_sid()?;
        if let Some(why) = untrusted_access(&real, &user, DIR_RIGHTS, "change")? {
            anyhow::bail!("Refusing to use {shown}: {why}");
        }
    }
    Ok(())
}

/// Refuse a directory that another user owns or can write to: they could
/// replace the stores, locks or plugins in it.
///
/// The same goes for every directory the path passes through, as written and
/// through each symlink it follows, since another user who can rename entries
/// in one of them could swap what the path leads to. Each such directory must
/// belong to you or root and be writable by no one else, unless it is sticky
/// like `/tmp` (then only an entry's owner can rename it), and every symlink
/// followed must belong to you or root.
#[cfg(unix)]
pub fn check_private_dir(dir: &Path) -> Result<()> {
    use std::ffi::OsString;
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
    let absolute = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir()
            .context("Failed to read the current directory")?
            .join(dir)
    };
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
        // Check the directory we are in before any step, `..` included:
        // leaving a directory through `..` depends on it as much as entering
        // one of its entries does
        let holder = stat(&current)?;
        if !owned_by_you_or_root(&holder, uid)
            || !only_you_can_write(&holder, true)
            || acl_allows_others(&current)
        {
            anyhow::bail!(
                "Refusing to use {shown}: another user could replace it through {} \
                 (it must belong to you or root and not be writable by others){}",
                crate::display::escape_path(&current),
                outside_namespace_note(&holder)
            );
        }
        if name == ".." {
            current.pop();
            continue;
        }
        let next = current.join(&name);
        let entry = stat(&next)?;
        if entry.file_type().is_symlink() {
            // In a sticky directory the link's owner can still replace it
            if !owned_by_you_or_root(&entry, uid) {
                anyhow::bail!(
                    "Refusing to use {shown}: the symlink {} belongs to another user{}",
                    crate::display::escape_path(&next),
                    outside_namespace_note(&entry)
                );
            }
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
    if !meta.is_dir() || !owned_by_you_or_root(&meta, uid) {
        anyhow::bail!(
            "Refusing to use {shown}: it is not a directory owned by you (uid {uid}){}",
            outside_namespace_note(&meta)
        );
    }
    if !only_you_can_write(&meta, false) {
        anyhow::bail!(
            "Refusing to use {shown}: other users can write to it (fix with: chmod go-w '{shown}')"
        );
    }
    if acl_allows_others(&current) {
        anyhow::bail!(
            "Refusing to use {shown}: its access control list grants access beyond the mode \
             bits (see: ls -led '{shown}'; remove with: chmod -N '{shown}')"
        );
    }
    Ok(())
}

#[cfg(unix)]
fn owned_by_you_or_root(meta: &std::fs::Metadata, uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.uid() == uid || meta.uid() == 0
}

/// No one but you (and root) can change the entries of this directory: it
/// is writable by no one else, or (when `sticky_ok`) it is sticky, so only an
/// entry's owner can rename that entry. Group write is refused even for your
/// own private group: ACLs and other accounts with that primary group can
/// widen it in ways the mode bits do not show.
#[cfg(unix)]
fn only_you_can_write(meta: &std::fs::Metadata, sticky_ok: bool) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.mode() & 0o022 == 0 || (sticky_ok && meta.mode() & 0o1000 != 0)
}

/// Whether a macOS access control list on `path` has an allow entry, which
/// can grant write access that the mode bits do not show. (Deny entries,
/// like the one macOS puts on home folders, only take access away.) On Linux
/// an ACL cannot grant more than the group bits allow, so the mode check
/// covers it.
#[cfg(target_os = "macos")]
pub(crate) fn acl_allows_others(path: &Path) -> bool {
    use std::ffi::{c_char, c_int, c_void, CString};
    use std::os::unix::ffi::OsStrExt;

    const ACL_TYPE_EXTENDED: c_int = 0x100;
    const ACL_FIRST_ENTRY: c_int = 0;
    const ACL_NEXT_ENTRY: c_int = -1;
    const ACL_EXTENDED_ALLOW: c_int = 1;
    extern "C" {
        fn acl_get_link_np(path: *const c_char, kind: c_int) -> *mut c_void;
        fn acl_get_entry(acl: *mut c_void, entry_id: c_int, entry: *mut *mut c_void) -> c_int;
        fn acl_get_tag_type(entry: *mut c_void, tag: *mut c_int) -> c_int;
        fn acl_free(obj: *mut c_void) -> c_int;
    }

    let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
        return true;
    };
    // SAFETY: the ACL is freed once; entries point into it and are not used
    // after that.
    unsafe {
        let acl = acl_get_link_np(c_path.as_ptr(), ACL_TYPE_EXTENDED);
        if acl.is_null() {
            return false; // no ACL
        }
        let mut allows = false;
        let mut entry: *mut c_void = std::ptr::null_mut();
        let mut which = ACL_FIRST_ENTRY;
        while acl_get_entry(acl, which, &mut entry) == 0 {
            which = ACL_NEXT_ENTRY;
            let mut tag: c_int = 0;
            if acl_get_tag_type(entry, &mut tag) != 0 || tag == ACL_EXTENDED_ALLOW {
                allows = true;
                break;
            }
        }
        acl_free(acl);
        allows
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn acl_allows_others(_path: &Path) -> bool {
    false
}

/// In a user namespace that maps only you (some sandboxes), files of every
/// other account, root included, show as the overflow uid 65534.
#[cfg(unix)]
fn outside_namespace_note(meta: &std::fs::Metadata) -> &'static str {
    use std::os::unix::fs::MetadataExt;
    if meta.uid() == 65534 {
        "; it shows as uid 65534, an account outside this user namespace, as inside \
         some sandboxes, which meta cannot tell apart from another user"
    } else {
        ""
    }
}

/// Refuse a directory that an account other than you, SYSTEM,
/// Administrators or TrustedInstaller owns or may change, or that sits in a
/// directory where such an account could rename or replace it. A link or
/// junction on the way is refused too: whoever controls it could point it
/// elsewhere after this check.
#[cfg(windows)]
pub fn check_private_dir(dir: &Path) -> Result<()> {
    use crate::win_acl::{current_user_sid, untrusted_access, ANCESTOR_RIGHTS, DIR_RIGHTS};

    let shown = crate::display::escape_path(dir);
    let user = current_user_sid()?;
    // Windows resolves `..` by the name, so the absolute path is the one used
    let written = std::path::absolute(dir).with_context(|| format!("Failed to resolve {shown}"))?;
    let mut ancestors: Vec<&Path> = written.ancestors().collect();
    ancestors.reverse();
    let mut holder: Option<&Path> = None;
    for path in ancestors {
        if let Some(holder) = holder {
            if let Some(why) = untrusted_access(holder, &user, ANCESTOR_RIGHTS, "change")? {
                anyhow::bail!(
                    "Refusing to use {shown}: another user could replace it through {} ({why})",
                    crate::display::escape_path(holder)
                );
            }
            let meta = std::fs::symlink_metadata(path)
                .with_context(|| format!("Failed to check {shown}"))?;
            if meta.file_type().is_symlink() {
                anyhow::bail!(
                    "Refusing to use {shown}: {} is a link or junction; use the real directory",
                    crate::display::escape_path(path)
                );
            }
        }
        holder = Some(path);
    }
    if let Some(why) = untrusted_access(&written, &user, DIR_RIGHTS, "change")? {
        anyhow::bail!(
            "Refusing to use {shown}: {why} (remove other accounts' write access, for \
             example with icacls, or use a directory in your profile)"
        );
    }
    Ok(())
}

/// No ownership model to check on other platforms.
#[cfg(not(any(unix, windows)))]
pub fn check_private_dir(_dir: &Path) -> Result<()> {
    Ok(())
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

        // `..` leaves a directory, which must be safe as well
        let err = check_private_dir(&tmp.path().join("links/../shared/mine"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("another user could replace it"), "{err}");

        // A sticky parent, like /tmp, keeps others from renaming it
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
        check_private_dir(&dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn test_windows_refuses_dir_others_can_change() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("shared");
        std::fs::create_dir(&dir).unwrap();
        check_private_dir(&dir).unwrap();

        // Let Everyone modify it
        let granted = std::process::Command::new("icacls")
            .arg(&dir)
            .args(["/grant", "*S-1-1-0:(OI)(CI)M"])
            .status()
            .unwrap();
        assert!(granted.success());
        let err = check_private_dir(&dir).unwrap_err().to_string();
        assert!(err.contains("S-1-1-0 may change it"), "{err}");

        // A private directory inside it can be replaced through it
        let inner = dir.join("inner");
        std::fs::create_dir(&inner).unwrap();
        let removed = std::process::Command::new("icacls")
            .arg(&inner)
            .args([
                "/inheritance:r",
                "/grant:r",
                "*S-1-5-32-544:(OI)(CI)F",
                "*S-1-5-18:(OI)(CI)F",
            ])
            .status()
            .unwrap();
        assert!(removed.success());
        let err = check_private_dir(&inner).unwrap_err().to_string();
        assert!(err.contains("another user could replace it"), "{err}");

        // A junction to a private directory is refused: it could be repointed
        let private = tmp.path().join("private");
        std::fs::create_dir(&private).unwrap();
        let junction = tmp.path().join("junction");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&private)
            .status()
            .unwrap();
        assert!(made.success());
        check_private_dir(&private).unwrap();
        let err = check_private_dir(&junction).unwrap_err().to_string();
        assert!(err.contains("is a link or junction"), "{err}");

        // A lock in a directory others can read is refused: they could hold it
        let readable = tmp.path().join("readable");
        std::fs::create_dir(&readable).unwrap();
        crate::lock::acquire(&readable.join("x.lock"), std::time::Duration::ZERO).unwrap();
        let granted = std::process::Command::new("icacls")
            .arg(&readable)
            .args(["/grant", "*S-1-1-0:(OI)(CI)R"])
            .status()
            .unwrap();
        assert!(granted.success());
        let err = crate::lock::acquire(&readable.join("x.lock"), std::time::Duration::ZERO)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("could hold the lock"), "{err}");

        // A grant that applies only to files in a private-looking directory
        // still reaches a store file created there
        let files_only = tmp.path().join("files-only");
        std::fs::create_dir(&files_only).unwrap();
        check_private_dir(&files_only).unwrap();
        let granted = std::process::Command::new("icacls")
            .arg(&files_only)
            .args(["/grant", "*S-1-1-0:(OI)(IO)M"])
            .status()
            .unwrap();
        assert!(granted.success());
        let store = files_only.join("store.json");
        crate::store::write_atomic(&store, &std::collections::HashMap::<String, String>::new())
            .unwrap();
        let err = crate::store::read::<std::collections::HashMap<String, String>>(&store)
            .unwrap_err()
            .to_string();
        assert!(err.contains("S-1-1-0 may change it"), "{err}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_macos_refuses_an_acl_that_allows_others() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("acl");
        std::fs::create_dir(&dir).unwrap();
        check_private_dir(&dir).unwrap();
        let added = std::process::Command::new("chmod")
            .args(["+a", "everyone allow add_file,delete_child"])
            .arg(&dir)
            .status()
            .unwrap();
        assert!(added.success());
        let err = check_private_dir(&dir).unwrap_err().to_string();
        assert!(err.contains("access control list"), "{err}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_macos_refuses_a_store_file_acl_that_allows_others() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store.json");
        crate::store::write_atomic(&store, &std::collections::HashMap::<String, String>::new())
            .unwrap();
        let added = std::process::Command::new("chmod")
            .args(["+a", "everyone allow write"])
            .arg(&store)
            .status()
            .unwrap();
        assert!(added.success());
        assert!(crate::store::read::<std::collections::HashMap<String, String>>(&store).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn test_trusted_file_refuses_a_plugin_others_can_change() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path().join("meta-x");
        std::fs::write(&plugin, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&plugin, std::fs::Permissions::from_mode(0o755)).unwrap();
        check_trusted_file(&plugin).unwrap();
        std::fs::set_permissions(&plugin, std::fs::Permissions::from_mode(0o757)).unwrap();
        assert!(check_trusted_file(&plugin).is_err());

        // A symlink to a plugin in a directory others can write is refused too
        let shared = tmp.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&plugin, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&plugin, shared.join("meta-x")).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        std::os::unix::fs::symlink(shared.join("meta-x"), &plugin).unwrap();
        assert!(check_trusted_file(&plugin).is_err());
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
