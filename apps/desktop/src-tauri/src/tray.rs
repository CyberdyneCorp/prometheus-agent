//! Background running (spec: desktop-app, Closing windows does not stop
//! the agent; Approval notifications never approve; Global hotkey
//! quick-ask): the tray with the pending approvals and running sessions,
//! the main window hiding to the tray when it closes, focus changes
//! reported to the page (an activation after an approval notification
//! focuses that request's card), and the quick-ask hotkey and panel.

use std::sync::Arc;

use pmt_desktop::Shell;
use pmt_desktop::attention::{MenuEntry, TrayState};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter as _, Manager as _, Runtime, WebviewWindow, WindowEvent};

use crate::app::{MAIN, QUICK_ASK, focus_main};

const TRAY: &str = "main";

fn menu<R: Runtime>(app: &AppHandle<R>, entries: &[MenuEntry]) -> tauri::Result<Menu<R>> {
    let menu = Menu::new(app)?;
    for entry in entries {
        match entry {
            MenuEntry::Heading(text) => {
                menu.append(&PredefinedMenuItem::separator(app)?)?;
                menu.append(&MenuItem::new(app, text, false, None::<&str>)?)?;
            }
            MenuEntry::Open { id, label } => {
                menu.append(&MenuItem::with_id(app, id, label, true, None::<&str>)?)?;
            }
            MenuEntry::Show => {
                menu.append(&PredefinedMenuItem::separator(app)?)?;
                menu.append(&MenuItem::with_id(
                    app,
                    "show",
                    "Show Prometheus",
                    true,
                    None::<&str>,
                )?)?;
            }
            MenuEntry::Quit => {
                menu.append(&MenuItem::with_id(
                    app,
                    "quit",
                    "Quit (the agent keeps running)",
                    true,
                    None::<&str>,
                )?)?;
            }
        }
    }
    Ok(menu)
}

/// Starts the tray icon; with `tap_to_summon` a left click (a tap)
/// summons the HUD instead of opening the menu.
pub(crate) fn start<R: Runtime>(app: &AppHandle<R>, tap_to_summon: bool) -> tauri::Result<()> {
    let mut tray = TrayIconBuilder::with_id(TRAY)
        .tooltip("Prometheus")
        .menu(&menu(app, &TrayState::default().menu())?)
        .show_menu_on_left_click(!tap_to_summon)
        .on_menu_event(|app, event| menu_event(app, event.id().as_ref()));
    if tap_to_summon {
        tray = tray.on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let _ = tray
                    .app_handle()
                    .emit_to(MAIN, crate::hud::SUMMON_EVENT, ());
            }
        });
    }
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

fn menu_event<R: Runtime>(app: &AppHandle<R>, id: &str) {
    match id {
        // Quitting the app never stops the daemon or its turns.
        "quit" => app.exit(0),
        "show" => focus_main(app),
        other => {
            if let Some(session_id) = other.strip_prefix("open:") {
                focus_main(app);
                let _ = app.emit_to(
                    MAIN,
                    crate::native::OPEN_SESSION_EVENT,
                    serde_json::json!({"session_id": session_id}),
                );
            }
        }
    }
}

/// Replaces the tray menu.
pub(crate) fn set_menu<R: Runtime>(app: &AppHandle<R>, entries: &[MenuEntry]) -> tauri::Result<()> {
    if let Some(tray) = app.tray_by_id(TRAY) {
        tray.set_menu(Some(menu(app, entries)?))?;
    }
    Ok(())
}

/// The main window hides to the tray when it closes (with
/// `desktop.keep_in_tray` and a tray), and tells its page about focus.
pub(crate) fn watch_main<R: Runtime>(window: &WebviewWindow<R>, shell: Arc<Shell>) {
    let handle = window.clone();
    window.on_window_event(move |event| match event {
        WindowEvent::CloseRequested { api, .. } if shell.keeps_running_in_tray() => {
            api.prevent_close();
            let _ = handle.hide();
        }
        WindowEvent::Focused(focused) => {
            let _ = handle.emit_to(
                MAIN,
                "window-focus",
                serde_json::json!({"focused": focused}),
            );
            if *focused && let Some(approval) = shell.notices().activated() {
                // The app was activated after an approval notification:
                // the page focuses that card. Nothing is answered here.
                let _ = handle.emit_to(MAIN, "focus-request", approval);
            }
        }
        _ => {}
    });
}

/// Registers the quick-ask hotkey (a taken chord is reported by the
/// shell with a notification; the app keeps running without it).
pub(crate) fn register_hotkey<R: Runtime>(app: &AppHandle<R>, shell: &Arc<Shell>) {
    let state = shell.register_quick_ask();
    tracing::info!(?state, "quick-ask hotkey");
    // The HUD's snap chord goes through the same plugin; the handler
    // tells the two apart by the registered chord.
    if let pmt_desktop::features::Hotkey::Registered { chord } = shell.register_hud_snap()
        && let Ok(shortcut) = chord.parse::<tauri_plugin_global_shortcut::Shortcut>()
    {
        app.manage(crate::hud::SnapChord(shortcut));
    }
}

/// Shows the quick-ask panel, or hides it when it has focus.
pub(crate) fn toggle_quick_ask<R: Runtime>(app: &AppHandle<R>) {
    let Some(panel) = app.get_webview_window(QUICK_ASK) else {
        return;
    };
    if panel.is_visible().unwrap_or(false) && panel.is_focused().unwrap_or(false) {
        let _ = panel.hide();
        return;
    }
    let _ = panel.show();
    let _ = panel.set_focus();
    let _ = panel.emit_to(QUICK_ASK, "quick-ask-shown", ());
}
