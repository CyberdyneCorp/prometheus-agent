//! The Prometheus desktop app (spec: desktop-app): a thin Tauri shell over
//! the profile's gateway daemon. The shell's logic lives in `pmt-desktop`;
//! this crate binds it to the webview, the OS plugins and the windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod native;

fn main() {
    app::run();
}
