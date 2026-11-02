//! The full-screen terminal UI's screen side (spec: tui): what the TUI
//! shows and how keys edit it. It owns no session: the `prometheus` binary
//! connects it to the gateway daemon and feeds it the session's events.
//!
//! - [`app`]: the screen state ([`app::App`]) and its rendering;
//! - [`transcript`], [`composer`], [`history`], [`status`]: its parts;
//! - [`keys`]: configurable key bindings (`ui.tui.keys`, `/keys`);
//! - [`slash`]: which `/` input the TUI handles itself, which goes to
//!   `command.dispatch` and which is a prompt;
//! - [`banner`], [`palette`], [`switcher`]: the startup banner and the
//!   command palette and session switcher overlays;
//! - [`busy`]: the status line's busy indicator;
//! - [`math`]: LaTeX math in answers;
//! - [`theme`], [`background`]: colours (`ui.tui.theme`, `NO_COLOR`) and
//!   the terminal's background;
//! - [`terminal`]: entering and always restoring the terminal, and the
//!   panic report.

pub mod app;
pub mod background;
pub mod banner;
pub mod busy;
pub mod composer;
pub mod history;
pub mod keys;
pub mod math;
pub mod palette;
pub mod slash;
pub mod status;
pub mod switcher;
pub mod terminal;
pub mod theme;
pub mod transcript;
pub mod wrap;
