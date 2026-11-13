//! The machine integration behind the native command allowlist, through
//! the Tauri plugins' Rust APIs (their JavaScript commands are never
//! granted to the webview).

use std::path::{Path, PathBuf};

use pmt_desktop::Native;
use tauri::{AppHandle, Manager as _, Runtime};
use tauri_plugin_clipboard_manager::ClipboardExt as _;
use tauri_plugin_dialog::DialogExt as _;
use tauri_plugin_global_shortcut::GlobalShortcutExt as _;
use tauri_plugin_notification::NotificationExt as _;
use tauri_plugin_opener::OpenerExt as _;

/// The app's [`Native`].
pub(crate) struct TauriNative<R: Runtime> {
    app: AppHandle<R>,
}

impl<R: Runtime> TauriNative<R> {
    pub(crate) fn new(app: AppHandle<R>) -> Self {
        Self { app }
    }
}

fn text(e: impl std::fmt::Display) -> String {
    e.to_string()
}

impl<R: Runtime> Native for TauriNative<R> {
    fn open_url(&self, url: &str) -> Result<(), String> {
        self.app.opener().open_url(url, None::<&str>).map_err(text)
    }

    fn open_path(&self, path: &Path) -> Result<(), String> {
        self.app
            .opener()
            .open_path(path.to_string_lossy(), None::<&str>)
            .map_err(text)
    }

    fn reveal(&self, path: &Path) -> Result<(), String> {
        self.app.opener().reveal_item_in_dir(path).map_err(text)
    }

    fn pick(&self, directory: bool, title: Option<&str>) -> Option<PathBuf> {
        let mut dialog = self.app.dialog().file();
        if let Some(title) = title {
            dialog = dialog.set_title(title);
        }
        let picked = if directory {
            dialog.blocking_pick_folder()
        } else {
            dialog.blocking_pick_file()
        };
        picked.and_then(|p| p.into_path().ok())
    }

    fn notify(&self, title: &str, body: &str) -> Result<(), String> {
        self.app
            .notification()
            .builder()
            .title(title)
            .body(body)
            .show()
            .map_err(text)
    }

    fn set_badge(&self, window: &str, count: Option<u32>) -> Result<(), String> {
        let window = self
            .app
            .get_webview_window(window)
            .ok_or_else(|| format!("no window {window}"))?;
        window.set_badge_count(count.map(i64::from)).map_err(text)
    }

    fn register_hotkey(&self, chord: &str) -> Result<(), String> {
        self.app.global_shortcut().register(chord).map_err(text)
    }

    fn read_clipboard(&self) -> Result<String, String> {
        self.app.clipboard().read_text().map_err(text)
    }
}
