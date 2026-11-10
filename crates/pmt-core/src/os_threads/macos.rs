//! macOS threads from the Mach kernel: `task_threads` lists the task's
//! thread ports, `thread_info` gives each one's id
//! (`THREAD_IDENTIFIER_INFO`, the same id `pthread_threadid_np` returns)
//! and its name and run state (`THREAD_EXTENDED_INFO`). No safe wrapper
//! offers these calls; the module is allowlisted for `unsafe` in
//! health.toml.
#![allow(unsafe_code)]

use std::ffi::CStr;

use libc::{
    KERN_SUCCESS, TH_STATE_HALTED, TH_STATE_RUNNING, TH_STATE_STOPPED, TH_STATE_UNINTERRUPTIBLE,
    TH_STATE_WAITING, THREAD_EXTENDED_INFO, THREAD_EXTENDED_INFO_COUNT, THREAD_IDENTIFIER_INFO,
    THREAD_IDENTIFIER_INFO_COUNT, kern_return_t, mach_msg_type_number_t, mach_port_t,
    thread_act_array_t, thread_act_t, thread_extended_info, thread_identifier_info, vm_address_t,
    vm_size_t,
};

use super::OsThread;

// Declared here: libc only has deprecated spellings of the task port and
// lacks `mach_port_deallocate`.
unsafe extern "C" {
    static mach_task_self_: mach_port_t;
    fn mach_port_deallocate(task: mach_port_t, name: mach_port_t) -> kern_return_t;
}

fn task_self() -> mach_port_t {
    // SAFETY: the kernel sets this process-wide port before any Rust code
    // runs and never changes it; reading a plain integer static.
    unsafe { mach_task_self_ }
}

pub(super) fn current_id() -> u64 {
    let mut id = 0u64;
    // SAFETY: a null thread means the calling thread; `id` is a valid out
    // pointer for the call's duration.
    let status = unsafe { libc::pthread_threadid_np(0, &raw mut id) };
    if status == 0 { id } else { 0 }
}

pub(super) fn list() -> Option<Vec<OsThread>> {
    let task = task_self();
    let mut ports: thread_act_array_t = std::ptr::null_mut();
    let mut count: mach_msg_type_number_t = 0;
    // SAFETY: valid out pointers; on success the kernel hands us `count`
    // thread ports in memory it allocated in our address space.
    if unsafe { libc::task_threads(task, &raw mut ports, &raw mut count) } != KERN_SUCCESS {
        return None;
    }
    let len = usize::try_from(count).unwrap_or(0);
    // SAFETY: the kernel returned `count` initialised ports at `ports`,
    // valid until we deallocate them below.
    let threads: Vec<OsThread> = unsafe { std::slice::from_raw_parts(ports, len) }
        .iter()
        .filter_map(|&port| {
            let thread = describe(port);
            // SAFETY: each port is a send right `task_threads` gave us;
            // releasing it once is required and harmless.
            unsafe { mach_port_deallocate(task, port) };
            thread
        })
        .collect();
    let bytes = len * std::mem::size_of::<thread_act_t>();
    // SAFETY: frees the array `task_threads` allocated, exactly once.
    unsafe {
        libc::vm_deallocate(task, ports as vm_address_t, bytes as vm_size_t);
    }
    Some(threads)
}

fn describe(port: thread_act_t) -> Option<OsThread> {
    // SAFETY: zeroed plain C structs are valid values.
    let mut ident: thread_identifier_info = unsafe { std::mem::zeroed() };
    let mut count = THREAD_IDENTIFIER_INFO_COUNT;
    // SAFETY: the buffer is a `thread_identifier_info` and `count` its
    // size in integers, as the flavor requires.
    let status = unsafe {
        libc::thread_info(
            port,
            THREAD_IDENTIFIER_INFO as u32,
            (&raw mut ident).cast(),
            &raw mut count,
        )
    };
    if status != KERN_SUCCESS {
        return None;
    }
    // SAFETY: as above.
    let mut extended: thread_extended_info = unsafe { std::mem::zeroed() };
    let mut count = THREAD_EXTENDED_INFO_COUNT;
    // SAFETY: the buffer is a `thread_extended_info` and `count` its size
    // in integers, as the flavor requires.
    let status = unsafe {
        libc::thread_info(
            port,
            THREAD_EXTENDED_INFO as u32,
            (&raw mut extended).cast(),
            &raw mut count,
        )
    };
    let (name, state) = if status == KERN_SUCCESS {
        (name(&extended), Some(run_state(extended.pth_run_state)))
    } else {
        (None, None)
    };
    Some(OsThread {
        id: ident.thread_id,
        name,
        state,
    })
}

fn name(info: &thread_extended_info) -> Option<String> {
    let bytes: Vec<u8> = info.pth_name.iter().map(|&c| c.cast_unsigned()).collect();
    let name = CStr::from_bytes_until_nul(&bytes).ok()?.to_string_lossy();
    (!name.is_empty()).then(|| name.into_owned())
}

fn run_state(state: i32) -> String {
    match state {
        TH_STATE_RUNNING => "running".to_owned(),
        TH_STATE_STOPPED => "stopped".to_owned(),
        TH_STATE_WAITING => "waiting".to_owned(),
        TH_STATE_UNINTERRUPTIBLE => "uninterruptible wait".to_owned(),
        TH_STATE_HALTED => "halted".to_owned(),
        other => format!("state {other}"),
    }
}
