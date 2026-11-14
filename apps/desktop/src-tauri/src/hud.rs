//! HUD mode (spec: desktop-app, HUD mode): the focused chat detached into
//! a chrome-free, always-on-top floating bar. The bar is one more window
//! of the app page (`#hud=<profile>/<id>`) with the same guards as the
//! others; it moves while the user presses and holds it, resizes from its
//! edges, goes back to its default size and place on Reset, and snaps to
//! the pointer on the `desktop.hud.snap_hotkey` chord.

use pmt_desktop::hud::Bounds;
use tauri::{
    AppHandle, Manager as _, PhysicalPosition, PhysicalSize, Runtime, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder,
};
use tauri_plugin_global_shortcut::Shortcut;

/// The HUD bar's label (the capability file grants it the shell).
pub(crate) const HUD: &str = "hud";
/// The event that asks the main window to detach its chat (a tray tap).
pub(crate) const SUMMON_EVENT: &str = "hud-summon";

const WIDTH: f64 = 720.0;
const HEIGHT: f64 = 140.0;

/// The snap chord, once registered.
pub(crate) struct SnapChord(pub(crate) Shortcut);

fn bar<R: Runtime>(app: &AppHandle<R>) -> Option<WebviewWindow<R>> {
    app.get_webview_window(HUD)
}

/// Shows the bar on `fragment`, or closes it when it shows; whether it
/// now shows.
pub(crate) fn toggle<R: Runtime>(app: &AppHandle<R>, fragment: &str) -> tauri::Result<bool> {
    if let Some(window) = bar(app) {
        window.close()?;
        return Ok(false);
    }
    let window = WebviewWindowBuilder::new(
        app,
        HUD,
        WebviewUrl::App(format!("index.html{fragment}").into()),
    )
    .title("Prometheus HUD")
    .inner_size(WIDTH, HEIGHT)
    .always_on_top(true)
    .decorations(false)
    .resizable(true)
    .skip_taskbar(true)
    .on_navigation(pmt_desktop::navigation::allow_navigation)
    .on_new_window(|url, _| crate::app::new_window(&url))
    .build()?;
    place_default(&window)?;
    window.set_focus()?;
    Ok(true)
}

/// Top centre of the bar's monitor.
fn place_default<R: Runtime>(window: &WebviewWindow<R>) -> tauri::Result<()> {
    let scale = window.scale_factor()?;
    window.set_size(tauri::LogicalSize::new(WIDTH, HEIGHT))?;
    if let Some(monitor) = window.current_monitor()? {
        let area = monitor.size();
        let width = (WIDTH * scale).round();
        let x = (f64::from(area.width) - width) / 2.0;
        window.set_position(PhysicalPosition::new(x.max(0.0), (48.0 * scale).round()))?;
    }
    Ok(())
}

/// Back to the default size and place.
pub(crate) fn reset<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    bar(app).map_or(Ok(()), |window| place_default(&window))
}

/// Moves with the pointer while the button is held.
pub(crate) fn drag<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    bar(app).map_or(Ok(()), |window| window.start_dragging())
}

/// Moves the bar to the pointer.
pub(crate) fn snap<R: Runtime>(app: &AppHandle<R>) {
    let (Some(window), Ok(pointer)) = (bar(app), app.cursor_position()) else {
        return;
    };
    let size = window.outer_size().unwrap_or(PhysicalSize::new(0, 0));
    let x = pointer.x - f64::from(size.width) / 2.0;
    let _ = window.set_position(PhysicalPosition::new(x, pointer.y));
    let _ = window.show();
    let _ = window.set_focus();
}

/// The bar's place on the screen.
pub(crate) fn bounds<R: Runtime>(app: &AppHandle<R>) -> Option<Bounds> {
    window_bounds(&bar(app)?)
}

/// Where `window` is on the screen.
pub(crate) fn window_bounds<R: Runtime>(window: &tauri::WebviewWindow<R>) -> Option<Bounds> {
    let at = window.outer_position().ok()?;
    let size = window.outer_size().ok()?;
    Some(Bounds {
        x: i64::from(at.x),
        y: i64::from(at.y),
        width: i64::from(size.width),
        height: i64::from(size.height),
    })
}
