//! The Windows side of `data_dir::check_private_dir`: read a directory's
//! owner and access list, and report an account other than you, SYSTEM,
//! Administrators or TrustedInstaller that owns it or holds given rights on it.

use anyhow::{bail, Result};
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    GetAce, GetTokenInformation, TokenUser, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
    DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, TOKEN_QUERY,
    TOKEN_USER,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACCESS_ALLOWED_OBJECT_ACE_TYPE: u8 = 5;
const ACCESS_ALLOWED_CALLBACK_ACE_TYPE: u8 = 9;
const ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE: u8 = 11;
const INHERIT_ONLY_ACE: u8 = 0x08;

const FILE_READ_DATA: u32 = 0x1;
const FILE_ADD_FILE: u32 = 0x2;
const FILE_ADD_SUBDIRECTORY: u32 = 0x4;
const FILE_DELETE_CHILD: u32 = 0x40;
const DELETE: u32 = 0x1_0000;
const WRITE_DAC: u32 = 0x4_0000;
const WRITE_OWNER: u32 = 0x8_0000;
const GENERIC_ALL: u32 = 0x1000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const GENERIC_READ: u32 = 0x8000_0000;

/// Rights on the data directory itself that would let an account change
/// what it holds, or who may.
pub const DIR_RIGHTS: u32 = FILE_ADD_FILE
    | FILE_ADD_SUBDIRECTORY
    | FILE_DELETE_CHILD
    | DELETE
    | WRITE_DAC
    | WRITE_OWNER
    | GENERIC_ALL
    | GENERIC_WRITE;

/// Rights on a directory above it that would let an account rename or
/// replace what it holds, or change who may.
pub const ANCESTOR_RIGHTS: u32 = FILE_DELETE_CHILD | DELETE | WRITE_DAC | WRITE_OWNER | GENERIC_ALL;

/// Rights that let an account open the files in a directory (their access
/// lists inherit from it), and so hold a lock on one.
pub const READ_RIGHTS: u32 = FILE_READ_DATA | GENERIC_READ | GENERIC_ALL;

/// SYSTEM, Administrators and TrustedInstaller, which can change anything anyway.
const SYSTEM_SIDS: [&str; 3] = [
    "S-1-5-18",
    "S-1-5-32-544",
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464",
];

/// The current user's SID, such as `S-1-5-21-…`.
pub fn current_user_sid() -> Result<String> {
    // SAFETY: the token handle is closed before returning; the buffer is
    // u64-aligned and as long as GetTokenInformation asked for, so it holds
    // a TOKEN_USER whose SID points into the buffer.
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            bail!(
                "Cannot read the current user: {}",
                std::io::Error::last_os_error()
            );
        }
        let mut len = 0u32;
        GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len);
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len);
        let error = std::io::Error::last_os_error();
        CloseHandle(token);
        if ok == 0 {
            bail!("Cannot read the current user: {error}");
        }
        let user = &*buf.as_ptr().cast::<TOKEN_USER>();
        sid_string(user.User.Sid)
    }
}

/// Why an account other than `user` and the system's own could `verb`
/// `dir`, if one could: it owns `dir`, there is no access list, or an entry
/// that applies to `dir` grants it any of `rights`.
pub fn untrusted_access(dir: &Path, user: &str, rights: u32, verb: &str) -> Result<Option<String>> {
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let trusted = |sid: &str| sid == user || SYSTEM_SIDS.contains(&sid);
    // SAFETY: GetNamedSecurityInfoW fills `owner` and `dacl` with pointers
    // into the descriptor `sd`, which stays allocated until LocalFree below;
    // GetAce returns pointers into that same access list.
    unsafe {
        let mut owner: PSID = null_mut();
        let mut dacl: *mut ACL = null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        let error = GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut sd,
        );
        if error != 0 {
            bail!(
                "Cannot read who may change {}: {}",
                crate::display::escape_path(dir),
                std::io::Error::from_raw_os_error(error as i32)
            );
        }
        let found = (|| -> Result<Option<String>> {
            let owner = sid_string(owner)?;
            if !trusted(&owner) {
                return Ok(Some(format!("it is owned by {owner}")));
            }
            if dacl.is_null() {
                return Ok(Some(
                    "it has no access list, so anyone may change it".into(),
                ));
            }
            for index in 0..u32::from((*dacl).AceCount) {
                let mut ace: *mut c_void = null_mut();
                if GetAce(dacl, index, &mut ace) == 0 {
                    bail!(
                        "Cannot read its access list: {}",
                        std::io::Error::last_os_error()
                    );
                }
                let header = &*ace.cast::<ACE_HEADER>();
                // Entries for children only grant nothing on this directory;
                // nor do deny or audit entries
                if header.AceFlags & INHERIT_ONLY_ACE != 0 {
                    continue;
                }
                match header.AceType {
                    ACCESS_ALLOWED_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_ACE_TYPE => {}
                    // Object entries carry their SID at another offset; not
                    // reading them, assume they may let someone in
                    ACCESS_ALLOWED_OBJECT_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE => {
                        return Ok(Some(
                            "it has an object access entry this check cannot read".into(),
                        ));
                    }
                    _ => continue,
                }
                let allowed = &*ace.cast::<ACCESS_ALLOWED_ACE>();
                if allowed.Mask & rights == 0 {
                    continue;
                }
                let sid = sid_string(std::ptr::addr_of!(allowed.SidStart).cast_mut().cast())?;
                if !trusted(&sid) {
                    return Ok(Some(format!("{sid} may {verb} it")));
                }
            }
            Ok(None)
        })();
        LocalFree(sd);
        found
    }
}

/// A SID as text, such as `S-1-5-18`.
unsafe fn sid_string(sid: PSID) -> Result<String> {
    let mut wide = null_mut();
    if ConvertSidToStringSidW(sid, &mut wide) == 0 {
        bail!("Cannot read a SID: {}", std::io::Error::last_os_error());
    }
    let mut len = 0;
    while *wide.add(len) != 0 {
        len += 1;
    }
    let text = String::from_utf16_lossy(std::slice::from_raw_parts(wide, len));
    LocalFree(wide.cast());
    Ok(text)
}
