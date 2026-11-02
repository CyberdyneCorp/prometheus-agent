//! The full-screen terminal UI's screen side (spec: tui): what the TUI
//! shows and how keys edit it. It owns no session: the `prometheus` binary
//! connects it to the gateway daemon and feeds it the session's events.
//!
//! - [`app`]: the screen state ([`app::App`]) and its rendering;
//! - [`transcript`], [`composer`], [`history`], [`status`]: its parts;
//! - [`keys`]: configurable key bindings (`ui.tui.keys`, `/keys`);
//! - [`slash`]: which `/` input the TUI handles itself, which goes to
//!   `command.dispatch` and which is a prompt;
//! - [`theme`]: colours (`ui.tui.theme`, `NO_COLOR`);
//! - [`terminal`]: entering and always restoring the terminal, and the
//!   panic report.

pub mod app;
pub mod composer;
pub mod history;
pub mod keys;
pub mod slash;
pub mod status;
pub mod terminal;
pub mod theme;
pub mod transcript;
pub mod wrap;
