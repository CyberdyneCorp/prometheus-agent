//! The `AppContainer` profile of one sandbox and the file ACL entries that
//! give its SID the policy's roots. FFI module allowlisted in health.toml.
//!
//! An `AppContainer` token reaches only objects whose ACL names its SID (or
//! `ALL APPLICATION PACKAGES`, which covers the Windows and Program Files
//! trees). Each sandbox creates a uniquely named profile, adds allow
//! entries for its carved roots (inheritable, or list-only on directories
//! that lead to a credential store), and on drop revokes every entry for its
//! SID and deletes the profile, so grants never leak to another session.

#![allow(unsafe_code)]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};

use super::acl;

use windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS;
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{
    FreeSid, NO_INHERITANCE, PSID, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
};

/// Access a root grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Access {
    Read,
    Write,
    /// List this directory only; nothing is inherited below it.
    List,
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
        let mask = match access {
            Access::Read | Access::List => FILE_GENERIC_READ | FILE_GENERIC_EXECUTE,
            Access::Write => FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE,
        };
        let inheritance = if path.is_dir() && access != Access::List {
            SUB_CONTAINERS_AND_OBJECTS_INHERIT
        } else {
            NO_INHERITANCE
        };
        self.touched.push(path.to_path_buf());
        acl::grant(path, self.sid, mask, inheritance)
            .map_err(|code| format!("{}: ACL update failed (error {code})", path.display()))
    }
}

impl Drop for AppContainer {
    fn drop(&mut self) {
        for path in &self.touched {
            acl::revoke(path, self.sid);
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

/// `text` as a NUL-terminated UTF-16 string.
pub(super) fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}
