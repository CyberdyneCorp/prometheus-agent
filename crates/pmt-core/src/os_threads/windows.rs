//! Windows threads from a Toolhelp snapshot (`TH32CS_SNAPTHREAD`, every
//! thread of the system filtered to this process), named with
//! `GetThreadDescription`. No safe wrapper offers these calls; the module
//! is allowlisted for `unsafe` in health.toml. Windows exposes no
//! scheduler state here, so `state` stays empty.
#![allow(unsafe_code)]

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThreadId, GetThreadDescription, OpenThread,
    THREAD_QUERY_LIMITED_INFORMATION,
};

use super::OsThread;

pub(super) fn current_id() -> u64 {
    // SAFETY: takes no arguments and cannot fail.
    u64::from(unsafe { GetCurrentThreadId() })
}

pub(super) fn list() -> Option<Vec<OsThread>> {
    // SAFETY: plain flags; the result is checked before use.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return None;
    }
    // SAFETY: takes no arguments and cannot fail.
    let pid = unsafe { GetCurrentProcessId() };
    let mut threads = Vec::new();
    let mut entry = THREADENTRY32 {
        dwSize: u32::try_from(std::mem::size_of::<THREADENTRY32>()).unwrap_or(0),
        cntUsage: 0,
        th32ThreadID: 0,
        th32OwnerProcessID: 0,
        tpBasePri: 0,
        tpDeltaPri: 0,
        dwFlags: 0,
    };
    // SAFETY: a live snapshot handle and an entry whose `dwSize` is set,
    // as both calls require.
    let mut more = unsafe { Thread32First(snapshot, &raw mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid {
            threads.push(OsThread {
                id: u64::from(entry.th32ThreadID),
                name: description(entry.th32ThreadID),
                state: None,
            });
        }
        // SAFETY: as above.
        more = unsafe { Thread32Next(snapshot, &raw mut entry) } != 0;
    }
    // SAFETY: closes the snapshot handle we own, exactly once.
    unsafe { CloseHandle(snapshot) };
    Some(threads)
}

/// The thread's description (its name), when set.
fn description(id: u32) -> Option<String> {
    // SAFETY: plain arguments; the handle is checked before use.
    let handle: HANDLE = unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, id) };
    if handle.is_null() {
        return None;
    }
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: a live thread handle with query rights and a valid out
    // pointer; on success the OS allocates the string we free below.
    let status = unsafe { GetThreadDescription(handle, &raw mut text) };
    let name = if status >= 0 && !text.is_null() {
        let mut len = 0;
        // SAFETY: the OS returned a NUL-terminated UTF-16 string; reading
        // stops at the terminator.
        while unsafe { *text.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `len` units before the terminator are initialised.
        let units = unsafe { std::slice::from_raw_parts(text, len) };
        Some(String::from_utf16_lossy(units))
    } else {
        None
    };
    if !text.is_null() {
        // SAFETY: frees the string `GetThreadDescription` allocated, once.
        unsafe { LocalFree(text.cast()) };
    }
    // SAFETY: closes the thread handle we opened, exactly once.
    unsafe { CloseHandle(handle) };
    name.filter(|n| !n.is_empty())
}
