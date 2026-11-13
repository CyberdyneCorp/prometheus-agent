//! The Tauri application: one window whose page reaches the shell through
//! the single typed `shell` command, windows that never open themselves
//! and navigation pinned to the app's origin, and `prometheus://` links
//! handed to the page after parsing.

use std::path::PathBuf;
use std::sync::Arc;

use pmt_desktop::{Request, Shell, ShellConfig, deeplink, ensure, navigation};
use serde_json::Value;
use tauri::webview::NewWindowResponse;
use tauri::{
    AppHandle, Emitter as _, Manager as _, Runtime, State, WebviewUrl, WebviewWindowBuilder,
};

use crate::native::TauriNative;

/// The main window's label (the capability file grants it the shell).
pub(crate) const MAIN: &str = "main";
/// The event a parsed deep link reaches the page with.
pub(crate) const DEEP_LINK_EVENT: &str = "deep-link";

/// The webview's one entry point: a typed [`Request`]; anything else is
/// rejected and logged.
#[tauri::command]
pub(crate) async fn shell<R: Runtime>(
    webview: tauri::Webview<R>,
    shell: State<'_, Arc<Shell>>,
    request: Value,
) -> Result<Value, String> {
    let request: Request = serde_json::from_value(request).map_err(|e| {
        tracing::warn!(window = webview.label(), "rejected a native command: {e}");
        format!("rejected: {e}")
    })?;
    shell.handle(webview.label(), request).await
}

/// Runs the app.
pub(crate) fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let context = tauri::generate_context!();
    with_shell(plugins(tauri::Builder::default()))
        .setup(|app| {
            let shell = Shell::new(
                shell_config()?,
                Arc::new(TauriNative::new(app.handle().clone())),
            );
            app.manage(Arc::new(shell));
            open_main(app.handle())?;
            forward_deep_links(app.handle());
            Ok(())
        })
        .run(context)
        .expect("the desktop app failed to start");
}

/// `builder` with the OS plugins.
fn plugins<R: Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    builder
        // First: a second launch (a deep link) is handed to this instance.
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            focus_main(app);
            for link in args.iter().filter(|a| a.starts_with("prometheus://")) {
                deliver(app, link);
            }
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
}

/// `builder` with the `shell` command, the page's only one.
pub(crate) fn with_shell<R: Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    builder.invoke_handler(tauri::generate_handler![shell])
}

fn shell_config() -> Result<ShellConfig, Box<dyn std::error::Error>> {
    let env = pmt_core::env::LaunchEnv::from_process(&pmt_core::env::EnvPolicy::default())?;
    let home = pmt_config::home::resolve(None, &env)?;
    let user_home = env.user_home().cloned().unwrap_or_else(|| home.clone());
    Ok(ShellConfig {
        binary: bundled_binary(),
        home,
        user_home,
        ensure_bound: ensure::DEADLINE + ensure::GRACE,
    })
}

/// The `prometheus` binary bundled next to the app's executable; a
/// development build without one uses the one on `PATH`.
fn bundled_binary() -> PathBuf {
    let name = format!("prometheus{}", std::env::consts::EXE_SUFFIX);
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(&name)));
    match sibling {
        Some(path) if path.is_file() || !cfg!(debug_assertions) => path,
        _ => PathBuf::from(name),
    }
}

fn open_main<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    WebviewWindowBuilder::new(app, MAIN, WebviewUrl::App("index.html".into()))
        .title("Prometheus")
        .inner_size(1100.0, 760.0)
        .on_navigation(navigation::allow_navigation)
        .on_new_window(|url, _| new_window(&url))
        .build()?;
    Ok(())
}

/// Page content never opens a window: `window.open` and new-window links
/// are denied, and nothing launches an external browser for them.
fn new_window<R: Runtime>(url: &tauri::Url) -> NewWindowResponse<R> {
    if navigation::allow_new_window(url) {
        NewWindowResponse::Allow
    } else {
        tracing::warn!(url = url.as_str(), "denied a window opened by page content");
        NewWindowResponse::Deny
    }
}

fn focus_main<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window(MAIN) {
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn forward_deep_links<R: Runtime>(app: &AppHandle<R>) {
    use tauri_plugin_deep_link::DeepLinkExt as _;
    let handle = app.clone();
    app.deep_link().on_open_url(move |event| {
        for url in event.urls() {
            deliver(&handle, url.as_str());
        }
    });
    if let Ok(Some(urls)) = app.deep_link().get_current() {
        for url in urls {
            deliver(app, url.as_str());
        }
    }
}

/// Parses a link and hands what it opens to the page; unknown links are
/// logged and dropped by the parser.
pub(crate) fn deliver<R: Runtime>(app: &AppHandle<R>, raw: &str) {
    if let Some(link) = deeplink::parse(raw) {
        let _ = app.emit_to(MAIN, DEEP_LINK_EVENT, link);
    }
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
