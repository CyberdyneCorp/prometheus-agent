//! The launcher side: starts the confined child inside the `AppContainer`
//! and a Job Object, then waits and passes its exit code through. FFI module
//! allowlisted in health.toml.
//!
//! The launcher inherits the stdio, working directory and environment the
//! caller set on its `Command`, and hands exactly its three standard handles
//! to the child. The child is created suspended, put in a job that kills it
//! when the launcher dies (`KILL_ON_JOB_CLOSE`), may not reach the desktop,
//! clipboard or global atoms, and, without `process.spawn`, may not start
//! other processes; then it is resumed.

#![allow(unsafe_code)]

use std::ffi::OsString;
use std::mem::size_of;
use std::ptr;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, LocalFree,
    SetHandleInformation,
};
use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows_sys::Win32::Security::Isolation::DeriveAppContainerSidFromAppContainerName;
use windows_sys::Win32::Security::{FreeSid, PSID, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_UILIMIT_DESKTOP, JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
    JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
    JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
    JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicUIRestrictions, JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
    PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

use super::args::{LaunchSpec, command_line};
use super::container::wide;

/// `SE_GROUP_ENABLED` (winnt.h), the attribute of an enabled capability.
const SE_GROUP_ENABLED: u32 = 4;
/// The `internetClient` capability SID: outbound connections to public
/// addresses, not to the private network or loopback.
const INTERNET_CLIENT: &str = "S-1-15-3-1";

/// Runs the launcher; returns the child's exit code.
pub(crate) fn run(spec: &LaunchSpec) -> Result<u32, String> {
    let container = Sid::of_container(&spec.container)?;
    let capability = if spec.internet {
        Some(Sid::parse(INTERNET_CLIENT)?)
    } else {
        None
    };
    let mut capabilities: Vec<SID_AND_ATTRIBUTES> = capability
        .iter()
        .map(|sid| SID_AND_ATTRIBUTES {
            Sid: sid.0,
            Attributes: SE_GROUP_ENABLED,
        })
        .collect();
    let security = SECURITY_CAPABILITIES {
        AppContainerSid: container.0,
        Capabilities: capabilities.as_mut_ptr(),
        CapabilityCount: u32::try_from(capabilities.len()).unwrap_or(0),
        Reserved: 0,
    };
    let handles = std_handles();
    let mut attributes = Attributes::new(2)?;
    attributes.set(
        PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
        &raw const security,
        size_of::<SECURITY_CAPABILITIES>(),
    )?;
    if !handles.is_empty() {
        attributes.set(
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            handles.as_ptr(),
            size_of::<HANDLE>() * handles.len(),
        )?;
    }
    let job = Job::new(spec.spawn)?;
    let child = create(spec, &handles, &mut attributes)?;
    job.assign(child.process)?;
    // SAFETY: a valid thread handle of the suspended child.
    if unsafe { ResumeThread(child.thread) } == u32::MAX {
        return Err(last_error("ResumeThread"));
    }
    child.wait()
}

struct Sid(PSID, fn(PSID));

impl Sid {
    fn of_container(name: &str) -> Result<Self, String> {
        let wide_name = wide(OsString::from(name).as_os_str());
        let mut sid: PSID = ptr::null_mut();
        let name_ptr = wide_name.as_ptr();
        // SAFETY: NUL-terminated name; `sid` is freed with FreeSid in Drop.
        let hr = unsafe { DeriveAppContainerSidFromAppContainerName(name_ptr, &raw mut sid) };
        if hr < 0 {
            return Err(format!("AppContainer SID for {name}: HRESULT {hr:#x}"));
        }
        Ok(Self(sid, |sid| {
            // SAFETY: allocated by DeriveAppContainerSidFromAppContainerName.
            unsafe { FreeSid(sid) };
        }))
    }

    fn parse(text: &str) -> Result<Self, String> {
        let wide_text = wide(OsString::from(text).as_os_str());
        let mut sid: PSID = ptr::null_mut();
        // SAFETY: NUL-terminated string; `sid` is freed with LocalFree.
        if unsafe { ConvertStringSidToSidW(wide_text.as_ptr(), &raw mut sid) } == 0 {
            return Err(last_error("ConvertStringSidToSidW"));
        }
        Ok(Self(sid, |sid| {
            // SAFETY: allocated by ConvertStringSidToSidW.
            unsafe { LocalFree(sid) };
        }))
    }
}

impl Drop for Sid {
    fn drop(&mut self) {
        (self.1)(self.0);
    }
}

/// The launcher's standard handles that exist, made inheritable, without
/// duplicates (a handle list may name each handle once).
fn std_handles() -> Vec<HANDLE> {
    let mut handles: Vec<HANDLE> = Vec::new();
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: GetStdHandle has no preconditions.
        let handle = unsafe { GetStdHandle(which) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE || handles.contains(&handle) {
            continue;
        }
        // SAFETY: a handle owned by this process; only its inherit flag changes.
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } != 0 {
            handles.push(handle);
        }
    }
    handles
}

/// A process/thread attribute list.
struct Attributes {
    buffer: Vec<u8>,
}

impl Attributes {
    fn new(count: u32) -> Result<Self, String> {
        let mut size = 0usize;
        // SAFETY: a null list with a size pointer only queries the size.
        unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), count, 0, &raw mut size) };
        let mut buffer = vec![0u8; size];
        // SAFETY: `buffer` has the size the first call asked for.
        if unsafe {
            InitializeProcThreadAttributeList(buffer.as_mut_ptr().cast(), count, 0, &raw mut size)
        } == 0
        {
            return Err(last_error("InitializeProcThreadAttributeList"));
        }
        Ok(Self { buffer })
    }

    fn set<T>(&mut self, attribute: u32, value: *const T, size: usize) -> Result<(), String> {
        // SAFETY: `value` points to `size` bytes that outlive CreateProcessW
        // (the caller keeps them alive for the whole of `run`).
        let ok = unsafe {
            UpdateProcThreadAttribute(
                self.buffer.as_mut_ptr().cast(),
                0,
                attribute as usize,
                value.cast(),
                size,
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if ok == 0 {
            Err(last_error("UpdateProcThreadAttribute"))
        } else {
            Ok(())
        }
    }
}

impl Drop for Attributes {
    fn drop(&mut self) {
        // SAFETY: the list was initialized in `new`.
        unsafe { DeleteProcThreadAttributeList(self.buffer.as_mut_ptr().cast()) };
    }
}

struct Job(HANDLE);

impl Job {
    fn new(spawn: bool) -> Result<Self, String> {
        // SAFETY: an anonymous job with default security.
        let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if handle.is_null() {
            return Err(last_error("CreateJobObjectW"));
        }
        let job = Self(handle);
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        if !spawn {
            limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            limits.BasicLimitInformation.ActiveProcessLimit = 1;
        }
        job.set(
            JobObjectExtendedLimitInformation,
            &raw const limits,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(),
        )?;
        let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
            UIRestrictionsClass: JOB_OBJECT_UILIMIT_DESKTOP
                | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
                | JOB_OBJECT_UILIMIT_EXITWINDOWS
                | JOB_OBJECT_UILIMIT_GLOBALATOMS
                | JOB_OBJECT_UILIMIT_HANDLES
                | JOB_OBJECT_UILIMIT_READCLIPBOARD
                | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
                | JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
        };
        job.set(
            JobObjectBasicUIRestrictions,
            &raw const ui,
            size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>(),
        )?;
        Ok(job)
    }

    fn set<T>(&self, class: i32, value: *const T, size: usize) -> Result<(), String> {
        let size = u32::try_from(size).map_err(|e| e.to_string())?;
        // SAFETY: `value` points to a structure of `size` bytes for `class`.
        if unsafe { SetInformationJobObject(self.0, class, value.cast(), size) } == 0 {
            return Err(last_error("SetInformationJobObject"));
        }
        Ok(())
    }

    fn assign(&self, process: HANDLE) -> Result<(), String> {
        // SAFETY: both handles are valid and owned by this process.
        if unsafe { AssignProcessToJobObject(self.0, process) } == 0 {
            return Err(last_error("AssignProcessToJobObject"));
        }
        Ok(())
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: closed once. Closing the last handle kills the job's
        // processes (KILL_ON_JOB_CLOSE), which is the point when the
        // launcher exits early.
        unsafe { CloseHandle(self.0) };
    }
}

struct Child {
    process: HANDLE,
    thread: HANDLE,
}

impl Child {
    fn wait(&self) -> Result<u32, String> {
        let mut code = 0u32;
        // SAFETY: a valid process handle; `code` is a valid out pointer.
        let ok = unsafe {
            WaitForSingleObject(self.process, INFINITE);
            GetExitCodeProcess(self.process, &raw mut code)
        };
        if ok == 0 {
            Err(last_error("GetExitCodeProcess"))
        } else {
            Ok(code)
        }
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        // SAFETY: both handles came from CreateProcessW and are closed once.
        unsafe {
            CloseHandle(self.thread);
            CloseHandle(self.process);
        }
    }
}

fn create(
    spec: &LaunchSpec,
    handles: &[HANDLE],
    attributes: &mut Attributes,
) -> Result<Child, String> {
    let mut line = command_line(&spec.program, &spec.args);
    let mut info = STARTUPINFOEXW::default();
    info.StartupInfo.cb = u32::try_from(size_of::<STARTUPINFOEXW>()).unwrap_or(0);
    info.lpAttributeList = attributes.buffer.as_mut_ptr().cast();
    if !handles.is_empty() {
        // SAFETY: GetStdHandle has no preconditions.
        unsafe {
            info.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            info.StartupInfo.hStdInput = GetStdHandle(STD_INPUT_HANDLE);
            info.StartupInfo.hStdOutput = GetStdHandle(STD_OUTPUT_HANDLE);
            info.StartupInfo.hStdError = GetStdHandle(STD_ERROR_HANDLE);
        }
    }
    let mut process = PROCESS_INFORMATION::default();
    // SAFETY: `line` is a mutable NUL-terminated buffer; `info` and the
    // attribute list it points to outlive the call; a null environment and
    // directory inherit the launcher's, which the caller configured.
    let ok = unsafe {
        CreateProcessW(
            ptr::null(),
            line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            i32::from(!handles.is_empty()),
            EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED,
            ptr::null(),
            ptr::null(),
            &raw const info.StartupInfo,
            &raw mut process,
        )
    };
    if ok == 0 {
        return Err(last_error("CreateProcessW"));
    }
    Ok(Child {
        process: process.hProcess,
        thread: process.hThread,
    })
}

fn last_error(call: &str) -> String {
    // SAFETY: no preconditions.
    let code = unsafe { GetLastError() };
    format!(
        "{call} failed: {}",
        std::io::Error::from_raw_os_error(i32::try_from(code).unwrap_or(-1))
    )
}
