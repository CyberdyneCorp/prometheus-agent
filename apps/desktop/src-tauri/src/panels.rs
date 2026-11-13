//! Plugin panels on their own origins (spec: desktop-app, Plugin UI
//! contributions are isolated): one URI scheme per plugin with a panel,
//! `plugin-<id>` (`http://plugin-<id>.localhost` on Windows), registered
//! before the windows open, so each plugin's frame has its own origin and
//! its own storage. The schemes serve only the panel document the shell
//! knows for that plugin, with a policy that forbids network requests.
//! Plugins installed while the app runs get their scheme at the next
//! launch.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pmt_desktop::Shell;
use pmt_desktop::navigation::APP_ORIGINS;
use pmt_desktop::panels::{self, Listing};
use tauri::utils::config::Csp;
use tauri::{Manager as _, Runtime};

/// The panels of every profile, by profile, from the bundled binary.
pub(crate) fn startup_listing(binary: &Path) -> Vec<(String, Listing)> {
    let binary = binary.to_owned();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = std::process::Command::new(binary)
            .args(["--json", "plugins", "panels", "--all-profiles"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
        let _ = tx.send(out);
    });
    let Ok(Ok(out)) = rx.recv_timeout(Duration::from_secs(15)) else {
        tracing::warn!("could not list the plugin panels; none load this launch");
        return Vec::new();
    };
    let Ok(all) = serde_json::from_slice::<Listing>(&out.stdout) else {
        return Vec::new();
    };
    let mut by_profile: BTreeMap<String, Listing> = BTreeMap::new();
    for panel in all.panels {
        by_profile
            .entry(panel.profile.clone())
            .or_default()
            .panels
            .push(panel);
    }
    by_profile.into_iter().collect()
}

fn ids(panels: &[(String, Listing)]) -> Vec<String> {
    let mut ids: Vec<String> = panels
        .iter()
        .flat_map(|(_, l)| l.panels.iter().map(|p| p.id.clone()))
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Lets the windows frame exactly the panels' origins.
pub(crate) fn allow_frames<R: Runtime>(
    context: &mut tauri::Context<R>,
    panels: &[(String, Listing)],
) {
    let origins: Vec<String> = ids(panels)
        .iter()
        .flat_map(|id| {
            let scheme = panels::scheme(id);
            [format!("{scheme}:"), format!("http://{scheme}.localhost")]
        })
        .collect();
    if origins.is_empty() {
        return;
    }
    if let Some(Csp::Policy(policy)) = &mut context.config_mut().app.security.csp {
        *policy = policy.replace(
            "frame-src 'none'",
            &format!("frame-src {}", origins.join(" ")),
        );
    }
}

/// `builder` with a scheme per panel.
pub(crate) fn schemes<R: Runtime>(
    mut builder: tauri::Builder<R>,
    panels: &[(String, Listing)],
) -> tauri::Builder<R> {
    for id in ids(panels) {
        let scheme = panels::scheme(&id);
        builder = builder.register_asynchronous_uri_scheme_protocol(
            scheme,
            move |ctx, _request, responder| {
                let document = ctx
                    .app_handle()
                    .try_state::<Arc<Shell>>()
                    .ok_or_else(|| "starting".to_owned())
                    .and_then(|shell| shell.panels().document(&id));
                let policy = format!(
                    "{}; frame-ancestors {}",
                    panels::POLICY,
                    APP_ORIGINS.join(" ")
                );
                let response = match document {
                    Ok(html) => tauri::http::Response::builder()
                        .status(200)
                        .header("Content-Type", "text/html; charset=utf-8")
                        .header("Content-Security-Policy", policy)
                        .body(html.into_bytes()),
                    Err(why) => {
                        tracing::warn!(plugin = id.as_str(), "no panel document: {why}");
                        tauri::http::Response::builder()
                            .status(404)
                            .body(Vec::new())
                    }
                };
                if let Ok(response) = response {
                    responder.respond(response);
                }
            },
        );
    }
    builder
}
