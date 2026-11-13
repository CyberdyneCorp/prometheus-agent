//! The Prometheus desktop app (spec: desktop-app): a thin Tauri shell over
//! the profile's gateway daemon. The shell's logic lives in `pmt-desktop`;
//! this crate binds it to the webview, the OS plugins and the windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod hud;
mod launch;
mod native;
mod panels;
mod tray;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let launch = match launch::Launch::read() {
        Ok(launch) => launch,
        Err(e) => {
            eprintln!("Prometheus Desktop could not start: {e}");
            std::process::exit(78);
        }
    };
    // The webview toolkit reads its switches from the environment before
    // the first window: start again with them when they are missing.
    if let Some(code) = launch.relaunch_with_environment() {
        std::process::exit(code);
    }
    launch.keep_launcher_entry();
    app::run(launch);
}
