//! Console code pages (Win32 console API; allowlisted for `unsafe` in
//! health.toml).

#![allow(unsafe_code)]

use windows_sys::Win32::System::Console::{
    GetConsoleCP, GetConsoleOutputCP, SetConsoleCP, SetConsoleOutputCP,
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
