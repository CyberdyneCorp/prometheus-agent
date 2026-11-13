//! The browser origins the loopback RPC listener admits (spec: desktop-app,
//! Daemon attachment through the bundled binary; workspace-architecture,
//! Ticket-authenticated loopback attachment): the desktop app's own
//! webview, which serves its page from `tauri://localhost` on macOS and
//! Linux and `http(s)://tauri.localhost` on Windows. No website can send
//! these origins, and a connection still needs a one-use ticket that only
//! the owner-only control channel hands out.

/// The desktop app's webview origins.
pub const DESKTOP_ORIGINS: &[&str] = &[
    "tauri://localhost",
    "http://tauri.localhost",
    "https://tauri.localhost",
];
