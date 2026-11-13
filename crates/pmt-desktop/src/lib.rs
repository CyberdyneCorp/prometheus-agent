//! The desktop shell's core (spec: desktop-app). The desktop app is a thin
//! native shell over the profile's gateway daemon: it owns only machine
//! integration behind an allowlist of typed native commands, while
//! sessions, agents, approvals and configuration live in the daemon. This
//! crate holds everything of the shell that does not need a webview, so it
//! is built and tested on every target; `apps/desktop/src-tauri` binds it
//! to Tauri.

pub mod attention;
pub mod below;
pub mod catalog;
pub mod connections;
pub mod deeplink;
pub mod dial;
pub mod endpoint;
pub mod ensure;
pub mod features;
pub mod hud;
pub mod launcher;
pub mod links;
pub mod navigation;
pub mod options;
pub mod panels;
pub mod platform;
pub mod profile_files;
pub mod quick_ask;
pub mod request;
pub mod shell;
pub mod skew;
pub mod update;
pub mod update_install;

pub use request::{Native, Request};
pub use shell::{Attach, Shell, ShellConfig};
