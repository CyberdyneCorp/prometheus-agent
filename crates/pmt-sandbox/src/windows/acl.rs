//! Adding and removing one SID's entry in a file's DACL without touching
//! anything else. FFI module allowlisted in health.toml.
//!
//! The DACL is rebuilt from the object's explicit entries only (minus any
//! entry for the SID, plus the new grant), then set with the object's own
//! protection state: the system re-inherits the parent's entries for an
//! unprotected DACL. Passing inherited entries back (as
//! `SetEntriesInAclW` output does) turned them into explicit copies on the
//! Windows ARM runners, which would leave the user's ACLs changed after
//! the sandbox ends.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::mem::size_of;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_INVALID_ACL, ERROR_SUCCESS, GetLastError, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    GetNamedSecurityInfoW, SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_FLAGS, ACE_HEADER, ACL, ACL_REVISION, AddAccessAllowedAceEx, AddAce,
    DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetLengthSid, GetSecurityDescriptorControl,
    INHERITED_ACE, InitializeAcl, OBJECT_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, SECURITY_DESCRIPTOR_CONTROL,
    UNPROTECTED_DACL_SECURITY_INFORMATION,
};

use super::container::wide;

/// Allowed and denied ACE types carry the SID right after the mask.
const ALLOWED_OR_DENIED: [u8; 2] = [0, 1];
const SID_OFFSET: usize = size_of::<ACE_HEADER>() + size_of::<u32>();
/// Size of an ACL header, and of an allowed ACE without its SID (the
/// struct's last field is the first DWORD of the SID).
#[allow(clippy::cast_possible_truncation, reason = "8 and 12 bytes")]
const ACL_HEADER: u32 = size_of::<ACL>() as u32;
#[allow(clippy::cast_possible_truncation, reason = "8 and 12 bytes")]
const ALLOWED_ACE_BASE: u32 = size_of::<ACCESS_ALLOWED_ACE>() as u32 - 4;
/// `AddAce` index meaning "at the end".
const APPEND: u32 = u32::MAX;

/// Gives `sid` `mask` on `path` (replacing an earlier entry for it).
pub(super) fn grant(path: &Path, sid: PSID, mask: u32, inheritance: ACE_FLAGS) -> Result<(), u32> {
    update(path, sid, Some((mask, inheritance)))
}

/// Removes every explicit entry for `sid` from `path`.
pub(super) fn revoke(path: &Path, sid: PSID) -> Result<(), u32> {
    update(path, sid, None)
}

fn update(path: &Path, sid: PSID, grant: Option<(u32, ACE_FLAGS)>) -> Result<(), u32> {
    let wide_path = wide(path.as_os_str());
    let descriptor = Descriptor::of(&wide_path)?;
    let mut acl = rebuild(descriptor.dacl, sid, grant)?;
    let info = DACL_SECURITY_INFORMATION | descriptor.protection();
    // SAFETY: the path is NUL-terminated and `acl` holds a valid ACL built
    // by InitializeAcl/AddAce for the duration of the call.
    let status = unsafe {
        SetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            info,
            ptr::null_mut(),
            ptr::null_mut(),
            acl.as_mut_ptr().cast(),
            ptr::null(),
        )
    };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(status)
    }
}

/// A file's security descriptor (DACL only), freed on drop.
struct Descriptor {
    raw: PSECURITY_DESCRIPTOR,
    dacl: *const ACL,
}

impl Descriptor {
    fn of(wide_path: &[u16]) -> Result<Self, u32> {
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut raw: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: NUL-terminated path; valid out pointers; `raw` (which owns
        // `dacl`) is freed with LocalFree in Drop.
        let status = unsafe {
            GetNamedSecurityInfoW(
                wide_path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &raw mut dacl,
                ptr::null_mut(),
                &raw mut raw,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(status);
        }
        let descriptor = Self { raw, dacl };
        // A NULL DACL grants everyone everything; replacing it with an
        // explicit list would lock other users out, so it is refused.
        if dacl.is_null() {
            Err(ERROR_INVALID_ACL)
        } else {
            Ok(descriptor)
        }
    }

    /// The flag that keeps the DACL's inheritance state as it is.
    fn protection(&self) -> OBJECT_SECURITY_INFORMATION {
        let mut control: SECURITY_DESCRIPTOR_CONTROL = 0;
        let mut rev = 0u32;
        // SAFETY: `raw` is a live descriptor; both out pointers are valid.
        let ok = unsafe { GetSecurityDescriptorControl(self.raw, &raw mut control, &raw mut rev) };
        if ok != 0 && control & SE_DACL_PROTECTED != 0 {
            PROTECTED_DACL_SECURITY_INFORMATION
        } else {
            UNPROTECTED_DACL_SECURITY_INFORMATION
        }
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by GetNamedSecurityInfoW, freed once.
        unsafe { LocalFree(self.raw) };
    }
}

/// The explicit entries of `old` except those for `sid`, as (pointer, size).
fn explicit_entries(old: *const ACL, sid: PSID) -> Result<Vec<(*const c_void, u32)>, u32> {
    // SAFETY: `old` is a valid, non-null ACL owned by a live descriptor.
    let count = unsafe { (*old).AceCount };
    let mut entries = Vec::new();
    for index in 0..u32::from(count) {
        let mut ace: *mut c_void = ptr::null_mut();
        // SAFETY: `index` < AceCount; GetAce returns a pointer into `old`.
        if unsafe { GetAce(old, index, &raw mut ace) } == 0 {
            return Err(ERROR_INVALID_ACL);
        }
        // SAFETY: every ACE starts with an ACE_HEADER.
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        if u32::from(header.AceFlags) & INHERITED_ACE != 0 {
            continue;
        }
        let names_sid = ALLOWED_OR_DENIED.contains(&header.AceType) && {
            // SAFETY: allowed/denied ACEs hold a SID at SID_OFFSET.
            unsafe { EqualSid(ace.cast::<u8>().add(SID_OFFSET).cast(), sid) != 0 }
        };
        if !names_sid {
            entries.push((ace.cast_const(), u32::from(header.AceSize)));
        }
    }
    Ok(entries)
}

/// A DWORD-aligned buffer holding the new ACL.
fn rebuild(old: *const ACL, sid: PSID, grant: Option<(u32, ACE_FLAGS)>) -> Result<Vec<u32>, u32> {
    let entries = explicit_entries(old, sid)?;
    // SAFETY: `sid` is a valid SID.
    let sid_len = unsafe { GetLengthSid(sid) };
    let grant_len = grant.map_or(0, |_| ALLOWED_ACE_BASE + sid_len);
    let total = ACL_HEADER + entries.iter().map(|(_, len)| len).sum::<u32>() + grant_len;
    let total = total.next_multiple_of(4);
    let mut buffer = vec![0u32; total as usize / 4];
    let acl = buffer.as_mut_ptr().cast::<ACL>();
    // SAFETY: `old` is valid; the new ACL gets the newer of the revisions.
    let revision = u32::from(unsafe { (*old).AclRevision }).max(ACL_REVISION);
    // SAFETY: `buffer` is `total` bytes, DWORD-aligned.
    if unsafe { InitializeAcl(acl, total, revision) } == 0 {
        return Err(last_error());
    }
    for (ace, len) in entries {
        // SAFETY: `ace` points to `len` bytes of a valid ACE inside `old`.
        if unsafe { AddAce(acl, revision, APPEND, ace, len) } == 0 {
            return Err(last_error());
        }
    }
    if let Some((mask, inheritance)) = grant {
        // Appended after the explicit entries, which keeps canonical order
        // (explicit denies, then explicit allows).
        // SAFETY: room for this ACE was reserved in `total`.
        if unsafe { AddAccessAllowedAceEx(acl, revision, inheritance, mask, sid) } == 0 {
            return Err(last_error());
        }
    }
    Ok(buffer)
}

fn last_error() -> u32 {
    // SAFETY: no preconditions.
    unsafe { GetLastError() }
}
