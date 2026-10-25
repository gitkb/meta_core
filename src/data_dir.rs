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
        if !owned_by_you_or_root(&holder, uid) || !only_you_can_write(&holder, uid, true) {
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
    if !only_you_can_write(&meta, uid, false) {
        anyhow::bail!(
            "Refusing to use {shown}: other users can write to it (fix with: chmod go-w '{shown}')"
        );
    }
    Ok(())
}

#[cfg(unix)]
fn owned_by_you_or_root(meta: &std::fs::Metadata, uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.uid() == uid || meta.uid() == 0
}

/// No one but you (and root) can change the entries of this directory: it is
/// not writable by others, or only for its group when that group is your own
/// private group, or (when `sticky_ok`) it is sticky, so only an entry's
/// owner can rename that entry.
#[cfg(unix)]
fn only_you_can_write(meta: &std::fs::Metadata, uid: u32, sticky_ok: bool) -> bool {
    use std::os::unix::fs::MetadataExt;
    if sticky_ok && meta.mode() & 0o1000 != 0 {
        return true;
    }
    meta.mode() & 0o002 == 0 && (meta.mode() & 0o020 == 0 || is_private_group(meta.gid(), uid))
}

/// Whether `gid` is the user's own private group, as on systems that give
/// every user a group of their own (and umask 002): the user's primary
/// group, named after the user, with no other members.
#[cfg(unix)]
fn is_private_group(gid: u32, uid: u32) -> bool {
    use std::ffi::CStr;
    use std::ptr::null_mut;

    // SAFETY: the reentrant lookups write only into the structs and buffers
    // passed in, and the strings read here point into those buffers.
    unsafe {
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut found: *mut libc::passwd = null_mut();
        let mut pw_buf = vec![0 as libc::c_char; 16 * 1024];
        if libc::getpwuid_r(uid, &mut pwd, pw_buf.as_mut_ptr(), pw_buf.len(), &mut found) != 0
            || found.is_null()
            || pwd.pw_gid != gid
        {
            return false;
        }
        let user = CStr::from_ptr(pwd.pw_name);

        let mut grp: libc::group = std::mem::zeroed();
        let mut found: *mut libc::group = null_mut();
        let mut gr_buf = vec![0 as libc::c_char; 64 * 1024];
        if libc::getgrgid_r(gid, &mut grp, gr_buf.as_mut_ptr(), gr_buf.len(), &mut found) != 0
            || found.is_null()
            || CStr::from_ptr(grp.gr_name) != user
        {
            return false;
        }
        let mut member = grp.gr_mem;
        while !member.is_null() && !(*member).is_null() {
            if CStr::from_ptr(*member) != user {
                return false;
            }
            member = member.add(1);
        }
        true
    }
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
            if let Some(why) = untrusted_access(holder, &user, ANCESTOR_RIGHTS)? {
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
    if let Some(why) = untrusted_access(&written, &user, DIR_RIGHTS)? {
        anyhow::bail!("Refusing to use {shown}: {why}");
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
    }

    #[cfg(unix)]
    #[test]
    fn test_group_write_is_fine_only_for_your_private_group() {
        use std::os::unix::fs::PermissionsExt;

        // A 0775 directory gets your primary group; whether that group is
        // private to you depends on the machine, and decides the outcome
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ws");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).unwrap();
        // SAFETY: no preconditions
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        let gid_of_dir = std::os::unix::fs::MetadataExt::gid(&std::fs::metadata(&dir).unwrap());
        let private = gid_of_dir == gid && is_private_group(gid, uid);
        assert_eq!(check_private_dir(&dir).is_ok(), private);
        // A group that is not yours never counts
        assert!(!is_private_group(gid.wrapping_add(1), uid));
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
