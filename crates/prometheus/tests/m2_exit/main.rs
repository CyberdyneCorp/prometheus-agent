//! The M2 exit criteria end to end (tasks 60.1-60.5): each module drives
//! the built binary through one exit criterion, chaining the surfaces M2
//! added the way a user meets them, against local fakes (the file-backed
//! test platform, the mock provider and its judge, fixture servers).

#![cfg(feature = "gateway")]

#[path = "../common/mod.rs"]
mod common;

#[cfg(unix)]
mod automation;
mod cron_delegation;
#[cfg(unix)]
mod daemon_clients;
mod extensions;
mod features;
#[cfg(unix)]
mod openai_sdk;
