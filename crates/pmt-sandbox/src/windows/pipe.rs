//! The named pipe between a proxied child's relay (inside its
//! `AppContainer`) and the launcher: its security descriptor and server
//! instances. FFI module allowlisted in health.toml.
//!
//! An `AppContainer` token passes an access check only when both its user
//! and its package SID are granted, and it runs at low integrity, so the
//! pipe's DACL names exactly this user and the container's SID, and its
//! label lets low integrity write. No other container, and no other user,
//! may open it.

#![allow(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::mem::size_of;
use std::os::windows::ffi::OsStringExt;
use std::ptr;

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::container::wide;

/// A security descriptor for the relay pipe of one container.
pub(super) struct Security(PSECURITY_DESCRIPTOR);

// SAFETY: the descriptor is an owned LocalAlloc block, only read after
// creation and freed once in Drop.
unsafe impl Send for Security {}
// SAFETY: as above, every use through &self is read-only.
unsafe impl Sync for Security {}

impl Security {
    /// Read and write for this process's user and the container `sid`.
    pub(super) fn for_container(sid: PSID) -> Result<Self, String> {
        let container = sid_text(sid)?;
        let user = current_user()?;
        let sddl = format!("D:P(A;;GA;;;{user})(A;;GA;;;{container})S:(ML;;NW;;;LW)");
        let text = wide(OsStr::new(&sddl));
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: NUL-terminated SDDL; the descriptor is freed with
        // LocalFree in Drop.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                SDDL_REVISION_1,
                &raw mut descriptor,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_error(
                "ConvertStringSecurityDescriptorToSecurityDescriptorW",
            ));
        }
        Ok(Self(descriptor))
    }

    /// A server instance of `name` with this descriptor; the first one
    /// fails if the name is already taken. Needs a Tokio runtime context.
    pub(super) fn create(&self, name: &OsStr, first: bool) -> std::io::Result<NamedPipeServer> {
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        };
        // SAFETY: `attributes` and the descriptor it points to are valid
        // for the call, which copies them into the new pipe.
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .create_with_security_attributes_raw(name, (&raw mut attributes).cast())
        }
    }
}

impl Drop for Security {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(self.0) };
    }
}

/// `sid` in `S-1-...` form.
fn sid_text(sid: PSID) -> Result<String, String> {
    let mut text: *mut u16 = ptr::null_mut();
    // SAFETY: a valid SID; the string is freed with LocalFree below.
    if unsafe { ConvertSidToStringSidW(sid, &raw mut text) } == 0 {
        return Err(last_error("ConvertSidToStringSidW"));
    }
    let mut len = 0usize;
    // SAFETY: the API returns a NUL-terminated string, read up to the NUL.
    let units = unsafe {
        while *text.add(len) != 0 {
            len += 1;
        }
        std::slice::from_raw_parts(text, len)
    };
    let result = OsString::from_wide(units).to_string_lossy().into_owned();
    // SAFETY: allocated by ConvertSidToStringSidW, freed once.
    unsafe { LocalFree(text.cast()) };
    Ok(result)
}

/// The SID of the user this process runs as.
fn current_user() -> Result<String, String> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: the pseudo handle of this process; `token` is closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(last_error("OpenProcessToken"));
    }
    let mut size = 0u32;
    // SAFETY: a null buffer only asks for the size.
    unsafe { GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &raw mut size) };
    // u64 elements keep the TOKEN_USER (pointer-aligned) aligned.
    let mut buffer = vec![0u64; (size as usize).div_ceil(8).max(1)];
    // SAFETY: `buffer` holds at least `size` bytes.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &raw mut size,
        )
    };
    let result = if ok == 0 {
        Err(last_error("GetTokenInformation"))
    } else {
        // SAFETY: the call filled the buffer with a TOKEN_USER whose SID
        // points into the same buffer, alive for this read.
        let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
        sid_text(user.User.Sid)
    };
    // SAFETY: opened above, closed once.
    unsafe { CloseHandle(token) };
    result
}

fn last_error(call: &str) -> String {
    // SAFETY: no preconditions.
    let code = unsafe { GetLastError() };
    format!(
        "{call} failed: {}",
        std::io::Error::from_raw_os_error(i32::try_from(code).unwrap_or(-1))
    )
}
