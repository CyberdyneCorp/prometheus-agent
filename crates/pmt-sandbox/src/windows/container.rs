//! The `AppContainer` profile of one sandbox and the file ACL entries that
//! give its SID the policy's roots. FFI module allowlisted in health.toml.
//!
//! An `AppContainer` token reaches only objects whose ACL names its SID (or
//! `ALL APPLICATION PACKAGES`, which covers the Windows and Program Files
//! trees). Each sandbox creates a uniquely named profile, adds inheritable
//! allow entries for its roots and deny entries for the credential stores
//! below them (an explicit deny on the store wins over the allow it would
//! inherit), and on drop revokes every entry for its SID and deletes the
//! profile, so grants never leak to another session.

#![allow(unsafe_code)]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_SUCCESS, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    DENY_ACCESS, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE,
    REVOKE_ACCESS, SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID,
    TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{
    ACE_FLAGS, ACL, DACL_SECURITY_INFORMATION, FreeSid, NO_INHERITANCE, PSECURITY_DESCRIPTOR, PSID,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ALL_ACCESS, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
};

/// Access a root grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Access {
    Read,
    Write,
    Deny,
}

/// A created `AppContainer` profile and the ACL entries made for it.
#[derive(Debug)]
pub(super) struct AppContainer {
    name: String,
    sid: PSID,
    touched: Vec<PathBuf>,
}

// SAFETY: the SID is an owned heap block only read (never mutated) after
// creation and freed once in Drop; sharing it across threads is sound.
unsafe impl Send for AppContainer {}
// SAFETY: as above, all access through &self is read-only.
unsafe impl Sync for AppContainer {}

/// `HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS)`.
#[allow(clippy::cast_possible_wrap)]
const ALREADY_EXISTS: i32 = (0x8007_0000 | ERROR_ALREADY_EXISTS) as i32;

static SEQUENCE: AtomicU32 = AtomicU32::new(0);

impl AppContainer {
    /// Creates a profile with a name unique to this process and sandbox.
    pub(super) fn create() -> Result<Self, String> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let name = format!(
            "prometheus.sandbox.{}.{}.{nanos}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let wide_name = wide(OsStr::new(&name));
        let display = wide(OsStr::new("Prometheus sandbox"));
        let mut sid: PSID = ptr::null_mut();
        // SAFETY: every string is NUL-terminated and outlives the call; no
        // capabilities are passed (count 0, null pointer); `sid` receives an
        // allocation we free with FreeSid.
        let hr = unsafe {
            CreateAppContainerProfile(
                wide_name.as_ptr(),
                display.as_ptr(),
                display.as_ptr(),
                ptr::null(),
                0,
                &raw mut sid,
            )
        };
        if hr == ALREADY_EXISTS {
            // SAFETY: as above; the derived SID is also freed with FreeSid.
            let hr = unsafe {
                DeriveAppContainerSidFromAppContainerName(wide_name.as_ptr(), &raw mut sid)
            };
            if hr < 0 {
                return Err(format!("AppContainer SID for {name}: HRESULT {hr:#x}"));
            }
        } else if hr < 0 {
            return Err(format!("AppContainer profile {name}: HRESULT {hr:#x}"));
        }
        Ok(Self {
            name,
            sid,
            touched: Vec::new(),
        })
    }

    /// The profile name the launcher derives the SID from.
    pub(super) fn name(&self) -> &str {
        &self.name
    }

    /// Adds an entry for this container's SID to `path`'s ACL.
    pub(super) fn grant(&mut self, path: &Path, access: Access) -> Result<(), String> {
        let (mode, mask) = match access {
            Access::Read => (GRANT_ACCESS, FILE_GENERIC_READ | FILE_GENERIC_EXECUTE),
            Access::Write => (
                GRANT_ACCESS,
                FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE,
            ),
            Access::Deny => (DENY_ACCESS, FILE_ALL_ACCESS),
        };
        let inheritance = if path.is_dir() {
            SUB_CONTAINERS_AND_OBJECTS_INHERIT
        } else {
            NO_INHERITANCE
        };
        self.touched.push(path.to_path_buf());
        set_entry(path, self.sid, mode, mask, inheritance)
            .map_err(|code| format!("{}: ACL update failed (error {code})", path.display()))
    }
}

impl Drop for AppContainer {
    fn drop(&mut self) {
        for path in &self.touched {
            // Best effort: a path removed since keeps no entry to revoke.
            let _ = set_entry(path, self.sid, REVOKE_ACCESS, 0, NO_INHERITANCE);
        }
        let name = wide(OsStr::new(&self.name));
        // SAFETY: `name` is NUL-terminated; `sid` came from the profile APIs
        // and is freed exactly once, here.
        unsafe {
            DeleteAppContainerProfile(name.as_ptr());
            FreeSid(self.sid);
        }
    }
}

/// Merges one explicit entry for `sid` into `path`'s DACL.
fn set_entry(
    path: &Path,
    sid: PSID,
    mode: i32,
    mask: u32,
    inheritance: ACE_FLAGS,
) -> Result<(), u32> {
    let wide_path = wide(path.as_os_str());
    let mut old_dacl: *mut ACL = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: the path is NUL-terminated; the out pointers are valid; the
    // returned descriptor (which owns `old_dacl`) is freed with LocalFree.
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &raw mut old_dacl,
            ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(status);
    }
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: mask,
        grfAccessMode: mode,
        grfInheritance: inheritance,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.cast(),
        },
    };
    let mut new_dacl: *mut ACL = ptr::null_mut();
    // SAFETY: one valid entry; `old_dacl` stays alive inside `descriptor`
    // until freed below; `new_dacl` is a LocalAlloc'd ACL we free.
    let status = unsafe { SetEntriesInAclW(1, &raw const entry, old_dacl, &raw mut new_dacl) };
    let status = if status == ERROR_SUCCESS {
        // SAFETY: path and the new DACL are valid for the call.
        unsafe {
            SetNamedSecurityInfoW(
                wide_path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                new_dacl,
                ptr::null(),
            )
        }
    } else {
        status
    };
    // SAFETY: both blocks were allocated by the security APIs above (or are
    // null, which LocalFree accepts).
    unsafe {
        LocalFree(new_dacl.cast());
        LocalFree(descriptor);
    }
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(status)
    }
}

/// `text` as a NUL-terminated UTF-16 string.
pub(super) fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}
