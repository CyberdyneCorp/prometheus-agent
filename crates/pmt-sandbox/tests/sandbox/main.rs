//! Behaviour of the OS sandbox and the terminal backend registry
//! (spec: security-boundaries, execution-environments). One test binary;
//! the per-OS modules run on their OS only.

mod backend;
mod common;
mod degrade;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix;
#[cfg(windows)]
mod windows;
