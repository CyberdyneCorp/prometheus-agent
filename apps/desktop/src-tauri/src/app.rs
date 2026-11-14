//! The Tauri application: the main window and the quick-ask panel, whose
//! pages reach the shell only through the single typed `shell` command;
//! windows that never open themselves and navigation pinned to the app's
//! origin; `prometheus://` links handed to the page after parsing; the
//! tray, the quick-ask hotkey and plugin panels on their own origins.

use std::path::PathBuf;
use std::sync::Arc;

use pmt_desktop::connections::service::ConnectionsConfig;
use pmt_desktop::connections::ssh::SshPrograms;
use pmt_desktop::{Request, Shell, ShellConfig, deeplink, ensure, navigation};
use serde_json::Value;
use tauri::webview::NewWindowResponse;
use tauri::{
    AppHandle, Emitter as _, Manager as _, Runtime, State, WebviewUrl, WebviewWindowBuilder,
};

use crate::launch::Launch;
use crate::native::TauriNative;

/// The main window's label (the capability file grants it the shell).
pub(crate) const MAIN: &str = "main";
/// The quick-ask panel's label.
pub(crate) const QUICK_ASK: &str = "quick-ask";
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
pub(crate) fn run(launch: Launch) {
    let mut context = tauri::generate_context!();
    let panels = crate::panels::startup_listing(&launch.binary);
    crate::panels::allow_frames(&mut context, &panels);
    let builder = crate::panels::schemes(plugins(tauri::Builder::default()), &panels);
    let app = with_shell(builder)
        .setup(move |app| {
            let version = app.package_info().version.to_string();
            let native = TauriNative::new(app.handle().clone(), launch.update_target());
            let mut shell = Shell::new(shell_config(&launch), Arc::new(native))
                .with_options(launch.options.clone())
                .with_updater(launch.updater(&version))
                .with_connections(connections_config(app.handle(), &launch)?);
            if launch.ignore_existing {
                shell = shell.without_local_daemon();
            }
            for (profile, listing) in &panels {
                shell.panels().set(profile, listing);
            }
            let shell = Arc::new(shell);
            app.manage(Arc::clone(&shell));
            open_main(app.handle(), &shell)?;
            open_quick_ask(app.handle())?;
            crate::tray::start(app.handle(), shell.options().hud.tap_to_summon)?;
            crate::tray::register_hotkey(app.handle(), &shell);
            forward_deep_links(app.handle(), &launch.links);
            Ok(())
        })
        .build(context)
        .expect("the desktop app failed to start");
    app.run(|handle, event| {
        if let tauri::RunEvent::Exit = event {
            // Quitting never stops the daemon; a staged update goes in now.
            crate::native::install_staged(handle);
        }
    });
}

/// The connection registry in the app's data directory, credentials in
/// the OS keychain.
fn connections_config<R: Runtime>(
    app: &tauri::AppHandle<R>,
    launch: &Launch,
) -> tauri::Result<ConnectionsConfig> {
    use tauri::Manager as _;
    let ssh = launch
        .options
        .options
        .ssh_path
        .as_deref()
        .map_or_else(SshPrograms::default, SshPrograms::at);
    Ok(ConnectionsConfig {
        dir: app.path().app_data_dir()?.join("connections"),
        keychain: Arc::new(pmt_config::secrets::keychain::OsKeychain),
        ssh,
    })
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
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state() != tauri_plugin_global_shortcut::ShortcutState::Pressed {
                        return;
                    }
                    let snap = app.try_state::<crate::hud::SnapChord>();
                    if snap.is_some_and(|chord| chord.0 == *shortcut) {
                        crate::hud::snap(app);
                    } else {
                        crate::tray::toggle_quick_ask(app);
                    }
                })
                .build(),
        )
}

/// `builder` with the `shell` command, the pages' only one.
pub(crate) fn with_shell<R: Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    builder.invoke_handler(tauri::generate_handler![shell])
}

fn shell_config(launch: &Launch) -> ShellConfig {
    ShellConfig {
        binary: launch.binary.clone(),
        home: launch.home.clone(),
        user_home: launch.user_home.clone(),
        ensure_bound: ensure::DEADLINE + ensure::GRACE,
    }
}

/// The `prometheus` binary bundled next to the app's executable; a
/// development build without one uses the one on `PATH`.
pub(crate) fn bundled_binary() -> PathBuf {
    let name = format!("prometheus{}", std::env::consts::EXE_SUFFIX);
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(&name)));
    match sibling {
        Some(path) if path.is_file() || !cfg!(debug_assertions) => path,
        _ => PathBuf::from(name),
    }
}

/// Runs the bundled CLI with this process's arguments in its place; the
/// exit code when it could not start.
pub(crate) fn run_bundled_cli() -> i32 {
    let mut command = std::process::Command::new(bundled_binary());
    command.args(std::env::args_os().skip(1));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let error = command.exec();
        eprintln!("prometheus: cannot run the bundled CLI: {error}");
        70
    }
    #[cfg(not(unix))]
    {
        command.status().map_or(70, |s| s.code().unwrap_or(1))
    }
}

/// Another app window on `fragment` of the page (spec: desktop-app,
/// Windows, tabs and panes): the same guards as the main window.
pub(crate) fn open_extra<R: Runtime>(app: &AppHandle<R>, fragment: &str) -> tauri::Result<()> {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    WebviewWindowBuilder::new(
        app,
        format!("window-{n}"),
        WebviewUrl::App(format!("index.html{fragment}").into()),
    )
    .title("Prometheus")
    .inner_size(900.0, 700.0)
    .on_navigation(|url| navigation::allow_navigation(url) || navigation::is_panel(url))
    .on_new_window(|url, _| new_window(&url))
    .initialization_script_for_all_frames(ANNOTATE_AGENT)
    .build()?;
    Ok(())
}

fn open_main<R: Runtime>(app: &AppHandle<R>, shell: &Arc<Shell>) -> tauri::Result<()> {
    let window = WebviewWindowBuilder::new(app, MAIN, WebviewUrl::App("index.html".into()))
        .title("Prometheus")
        .inner_size(1100.0, 760.0)
        .on_navigation(|url| navigation::allow_navigation(url) || navigation::is_panel(url))
        .on_new_window(|url, _| new_window(&url))
        .initialization_script_for_all_frames(ANNOTATE_AGENT)
        .build()?;
    crate::tray::watch_main(&window, Arc::clone(shell));
    Ok(())
}

/// The preview's annotate agent (spec: desktop-app, Annotate mode in the
/// in-app browser): in every frame, inert until the app page starts it in
/// the preview frame, so it reaches pages of any origin.
const ANNOTATE_AGENT: &str = include_str!("../../dist/annotate.js");

fn open_quick_ask<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    WebviewWindowBuilder::new(
        app,
        QUICK_ASK,
        WebviewUrl::App("index.html#quick-ask".into()),
    )
    .title("Quick ask")
    .inner_size(640.0, 220.0)
    .always_on_top(true)
    .decorations(false)
    .skip_taskbar(true)
    .visible(false)
    .on_navigation(|url| navigation::allow_navigation(url) || navigation::is_panel(url))
    .on_new_window(|url, _| new_window(&url))
    .build()?;
    Ok(())
}

/// Page content never opens a window: `window.open` and new-window links
/// are denied, and nothing launches an external browser for them.
pub(crate) fn new_window<R: Runtime>(url: &tauri::Url) -> NewWindowResponse<R> {
    if navigation::allow_new_window(url) {
        NewWindowResponse::Allow
    } else {
        tracing::warn!(url = url.as_str(), "denied a window opened by page content");
        NewWindowResponse::Deny
    }
}

/// Shows and focuses the main window.
pub(crate) fn focus_main<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window(MAIN) {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn forward_deep_links<R: Runtime>(app: &AppHandle<R>, from_argv: &[String]) {
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
    // `prometheus desktop` starts the app with the link as an argument.
    for link in from_argv {
        deliver(app, link);
    }
}

/// Parses a link and hands what it opens to the page (held until the page
/// is ready); unknown links are logged and dropped by the parser.
pub(crate) fn deliver<R: Runtime>(app: &AppHandle<R>, raw: &str) {
    let Some(link) = deeplink::parse(raw) else {
        return;
    };
    let ready = app
        .try_state::<Arc<Shell>>()
        .is_none_or(|shell| shell.arrived(link.clone()));
    if ready {
        let _ = app.emit_to(MAIN, DEEP_LINK_EVENT, link);
    }
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
