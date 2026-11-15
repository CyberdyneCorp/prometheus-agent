//! `prometheus acp` against a daemon on a test home (spec: acp-adapter):
//! the editor fixture in [`fixture`] drives the adapter over stdio as an
//! ACP client, one module per task of group 66.

#![cfg(feature = "acp")]

#[path = "../common/mod.rs"]
mod common;

mod cards;
mod editor;
mod fixture;
mod launch;
mod prompts;
mod sessions;
mod unattended;
