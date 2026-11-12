//! Console code pages and control events (Win32 console API; allowlisted
//! for `unsafe` in health.toml).

#![allow(unsafe_code)]

use windows_sys::Win32::System::Console::{
    CTRL_BREAK_EVENT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GenerateConsoleCtrlEvent, GetConsoleCP,
    GetConsoleMode, GetConsoleOutputCP, GetStdHandle, STD_OUTPUT_HANDLE, SetConsoleCP,
    SetConsoleMode, SetConsoleOutputCP,
};

/// `(input, output)`; `None` when the process has no console.
pub(crate) fn code_pages() -> Option<(u32, u32)> {
    // SAFETY: both calls take no arguments and only read console state;
    // they return 0 when there is no console.
    let (input, output) = unsafe { (GetConsoleCP(), GetConsoleOutputCP()) };
    (input != 0 && output != 0).then_some((input, output))
}

/// Sets both code pages (failures leave the console as it was).
pub(crate) fn set_code_pages((input, output): (u32, u32)) {
    // SAFETY: plain integer arguments; the calls fail harmlessly without a
    // console or with an unsupported code page.
    unsafe {
        SetConsoleCP(input);
        SetConsoleOutputCP(output);
    }
}

/// Sends Ctrl+Break to the console process group `group`.
pub(crate) fn ctrl_break(group: u32) -> std::io::Result<()> {
    // SAFETY: plain integer arguments; the call fails (returning 0, with
    // the reason in the thread's last error) when this process has no
    // console or `group` is not a process group on it.
    if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, group) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The output console's mode, `None` when stdout is not a console.
pub(crate) fn output_mode() -> Option<u32> {
    let mut mode = 0u32;
    // SAFETY: GetStdHandle takes a constant; GetConsoleMode writes one u32
    // to a valid pointer and fails (returning 0) for a handle that is not
    // a console, such as a redirected stdout.
    let ok = unsafe { GetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), &raw mut mode) };
    (ok != 0).then_some(mode)
}

/// Sets the output console's mode (a failure leaves it as it was).
pub(crate) fn set_output_mode(mode: u32) {
    // SAFETY: plain integer arguments on the process's own stdout handle;
    // the call fails harmlessly when it is not a console.
    unsafe {
        SetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), mode);
    }
}

/// The mode bit that makes the console interpret ANSI sequences.
pub(crate) const VIRTUAL_TERMINAL: u32 = ENABLE_VIRTUAL_TERMINAL_PROCESSING;
