//! The pet's overlay window (spec: pets, Desktop pet and pop-out overlay):
//! the page's `#pet` view in a transparent, undecorated, always-on-top
//! window kept out of the taskbar and the window switcher where the
//! platform allows, with no protocol connection of its own (its messages
//! go through the shell's `pet_relay`). Its place on the screen is kept in
//! the home (`pmt_desktop::pet`) whenever it moves.

use std::path::Path;

use pmt_desktop::pet::{OVERLAY, Place, save_place, saved_place};
use tauri::{AppHandle, Manager as _, PhysicalPosition, Runtime, WebviewUrl, WebviewWindowBuilder};

const WIDTH: f64 = 240.0;
const HEIGHT: f64 = 300.0;

/// Opens the overlay (at its saved place) or closes it; whether it shows.
pub(crate) fn overlay<R: Runtime>(
    app: &AppHandle<R>,
    home: &Path,
    open: bool,
) -> tauri::Result<bool> {
    let existing = app.get_webview_window(OVERLAY);
    if !open {
        if let Some(window) = existing {
            window.close()?;
        }
        return Ok(false);
    }
    if let Some(window) = existing {
        window.show()?;
        return Ok(true);
    }
    let window = WebviewWindowBuilder::new(app, OVERLAY, WebviewUrl::App("index.html#pet".into()))
        .title("Prometheus pet")
        .inner_size(WIDTH, HEIGHT)
        .transparent(true)
        .decorations(false)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible_on_all_workspaces(true)
        .resizable(false)
        .focused(false)
        .on_navigation(pmt_desktop::navigation::allow_navigation)
        .on_new_window(|url, _| crate::app::new_window(&url))
        .build()?;
    if let Some(place) = saved_place(home) {
        window.set_position(PhysicalPosition::new(place.x, place.y))?;
    }
    let home = home.to_path_buf();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::Moved(at) = event {
            save_place(&home, Place { x: at.x, y: at.y });
        }
    });
    Ok(true)
}

/// Shows the main window, or hides it when it shows and `toggle`;
/// whether it now shows.
pub(crate) fn main_window<R: Runtime>(app: &AppHandle<R>, toggle: bool) -> tauri::Result<bool> {
    let Some(main) = app.get_webview_window(crate::app::MAIN) else {
        return Ok(false);
    };
    if toggle && main.is_visible()? && !main.is_minimized()? {
        main.hide()?;
        return Ok(false);
    }
    crate::app::focus_main(app);
    Ok(true)
}
