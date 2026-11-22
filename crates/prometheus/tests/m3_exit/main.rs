//! The M3 exit criteria end to end (tasks 81.1-81.3): each module drives
//! the built binary through one exit criterion, chaining the surfaces M3
//! added the way a user meets them, against local fakes (the mock
//! provider, the desktop browser harness, a stand-in computer-use driver,
//! a mock Feishu platform, second daemons on loopback).

#![cfg(all(unix, feature = "dashboard", feature = "acp", feature = "evolve"))]

#[path = "../common/mod.rs"]
mod common;

#[allow(dead_code, unreachable_pub)]
#[path = "../acp/fixture.rs"]
mod acp_fixture;

mod a2a;
mod bots;
mod catalog;
mod clients;
mod computer_use;
mod kanban;
mod pets;
mod self_improvement;
