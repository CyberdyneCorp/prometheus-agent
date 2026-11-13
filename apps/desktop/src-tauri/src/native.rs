//! The machine integration behind the native command allowlist, through
//! the Tauri plugins' Rust APIs (their JavaScript commands are never
//! granted to the webview).

use std::path::{Path, PathBuf};

use pmt_desktop::Native;
use pmt_desktop::attention::MenuEntry;
use pmt_desktop::update_install::Target;
use tauri::{AppHandle, Emitter as _, Manager as _, Runtime};
use tauri_plugin_clipboard_manager::ClipboardExt as _;
use tauri_plugin_dialog::DialogExt as _;
use tauri_plugin_global_shortcut::GlobalShortcutExt as _;
use tauri_plugin_notification::NotificationExt as _;
use tauri_plugin_opener::OpenerExt as _;

/// The event that opens a session in the main window.
pub(crate) const OPEN_SESSION_EVENT: &str = "open-session";

/// The app's [`Native`].
pub(crate) struct TauriNative<R: Runtime> {
    app: AppHandle<R>,
    update: Option<(Target, PathBuf)>,
}

impl<R: Runtime> TauriNative<R> {
    pub(crate) fn new(app: AppHandle<R>, update: Option<(Target, PathBuf)>) -> Self {
        if let Some(update) = &update {
            app.manage(StagedUpdate(update.clone()));
        }
        Self { app, update }
    }
}

/// Where a staged update is installed on quit.
struct StagedUpdate((Target, PathBuf));

/// Installs the staged update, if any (the app is quitting).
pub(crate) fn install_staged<R: Runtime>(app: &AppHandle<R>) {
    let Some(state) = app.try_state::<StagedUpdate>() else {
        return;
    };
    let (target, dir) = &state.0;
    install_from(target, dir);
}

fn install_from(target: &Target, dir: &Path) {
    let Some(staged) = pmt_desktop::update::staged_in(dir) else {
        return;
    };
    match pmt_desktop::update_install::install(&staged, target, dir) {
        Ok(()) => tracing::info!(version = staged.version, "installed the staged update"),
        Err(e) => tracing::warn!("could not install the staged update: {e}"),
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

    fn set_tray(&self, menu: &[MenuEntry]) -> Result<(), String> {
        crate::tray::set_menu(&self.app, menu).map_err(text)
    }

    fn open_in_main(&self, profile: &str, session_id: &str) -> Result<(), String> {
        crate::app::focus_main(&self.app);
        self.app
            .emit_to(
                crate::app::MAIN,
                OPEN_SESSION_EVENT,
                serde_json::json!({"profile": profile, "session_id": session_id}),
            )
            .map_err(text)
    }

    fn open_window(&self, fragment: &str) -> Result<(), String> {
        crate::app::open_extra(&self.app, fragment).map_err(text)
    }

    fn hide_window(&self, window: &str) -> Result<(), String> {
        let window = self
            .app
            .get_webview_window(window)
            .ok_or_else(|| format!("no window {window}"))?;
        window.hide().map_err(text)
    }

    fn restart(&self) -> Result<(), String> {
        if let Some((target, dir)) = &self.update {
            install_from(target, dir);
        }
        self.app.restart();
    }

    fn toggle_hud(&self, fragment: &str) -> Result<bool, String> {
        crate::hud::toggle(&self.app, fragment).map_err(text)
    }

    fn reset_hud(&self) -> Result<(), String> {
        crate::hud::reset(&self.app).map_err(text)
    }

    fn drag_hud(&self) -> Result<(), String> {
        crate::hud::drag(&self.app).map_err(text)
    }

    fn window_below(&self) -> Option<pmt_desktop::hud::WindowBelow> {
        let bar = crate::hud::bounds(&self.app)?;
        pmt_desktop::hud::below(&self.facts(), &bar)
    }

    fn save_path(&self, name: &str) -> Option<PathBuf> {
        self.app
            .dialog()
            .file()
            .set_file_name(name)
            .blocking_save_file()
            .and_then(|p| p.into_path().ok())
    }

    fn facts(&self) -> pmt_desktop::platform::Facts {
        let x11 = std::env::var("GDK_BACKEND").is_ok_and(|b| b == "x11");
        pmt_desktop::platform::Facts::detect(x11)
    }
}
