//! The local terminal backend run for real under the host's OS sandbox
//! (spec: execution-environments). Shell-based tests need a POSIX shell;
//! Windows shell resolution has its own module.

#[cfg(unix)]
mod common;
#[cfg(unix)]
mod environment;
#[cfg(unix)]
mod foreground;
#[cfg(unix)]
mod processes;
#[cfg(unix)]
mod startup;
#[cfg(windows)]
mod windows;
