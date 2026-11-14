//! The shell's requests beyond attaching and links (spec: desktop-app):
//! updates, platform support, launch options, deep links held for the
//! window, approval notifications and the tray, the quick-ask panel,
//! plugin panels and the catalog install flow.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::attention::Approval;
use crate::catalog::{self, Entry, Kind};
use crate::panels::Listing;
use crate::request::Request;
use crate::shell::{Shell, ok, to_json};

/// How long a bundled-binary query may take.
const QUERY_BOUND: Duration = Duration::from_mins(1);
/// How long an install may take (downloads included).
const INSTALL_BOUND: Duration = Duration::from_mins(10);

/// What became of the quick-ask hotkey at launch.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Hotkey {
    /// `desktop.quick_ask.hotkey` is empty.
    Disabled,
    /// The window system offers no global shortcuts.
    Unsupported {
        /// Why.
        reason: String,
    },
    /// Registered.
    Registered {
        /// The chord.
        chord: String,
    },
    /// Another application owns the chord: the app runs without it.
    Taken {
        /// The chord.
        chord: String,
    },
}

/// Whether `request` is one of these.
pub(crate) fn owns(request: &Request) -> bool {
    matches!(
        request,
        Request::CheckUpdate
            | Request::BackgroundCheck
            | Request::ApplyUpdate
            | Request::RestartApp
            | Request::PlatformSupport
            | Request::InstallCli
            | Request::LaunchOptions
            | Request::TakeLinks
            | Request::NotifyApproval(_)
            | Request::RequestSettled { .. }
            | Request::SetTray(_)
            | Request::QuickAskTarget { .. }
            | Request::QuickAskUsed { .. }
            | Request::OpenInMain { .. }
            | Request::HidePanel
            | Request::NewWindow
            | Request::PopOut { .. }
            | Request::PluginPanels { .. }
            | Request::CatalogEntry { .. }
            | Request::CatalogInstall { .. }
            | Request::HudToggle { .. }
            | Request::HudReset
            | Request::HudDrag
            | Request::WindowBelow
            | Request::ReadWindowBelow
    )
}

impl Shell {
    /// Registers the quick-ask hotkey; when another application owns the
    /// chord, a notification names it and the app keeps running without.
    pub fn register_quick_ask(&self) -> Hotkey {
        let chord = self.options.options.quick_ask.hotkey.clone();
        let rows = crate::platform::probe(&self.native.facts());
        let unsupported = rows
            .iter()
            .find(|r| r.feature == crate::platform::GLOBAL_SHORTCUT && r.state == "unsupported");
        let state = if chord.is_empty() {
            Hotkey::Disabled
        } else if let Some(row) = unsupported {
            Hotkey::Unsupported {
                reason: row.reason.clone(),
            }
        } else if let Err(why) = self.native.register_hotkey(&chord) {
            tracing::warn!(
                chord = chord.as_str(),
                "the quick-ask hotkey is taken: {why}"
            );
            let _ = self.native.notify(
                "Quick ask hotkey unavailable",
                &format!("{chord} is already used by another application; Prometheus keeps running without the quick-ask hotkey."),
            );
            Hotkey::Taken { chord }
        } else {
            Hotkey::Registered { chord }
        };
        *self
            .hotkey
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(state.clone());
        state
    }

    /// Registers the HUD's snap-to-pointer chord (spec: desktop-app, HUD
    /// mode); on native Wayland placement is the compositor's, so the
    /// chord stays unregistered with that reason.
    pub fn register_hud_snap(&self) -> Hotkey {
        let chord = self.options.options.hud.snap_hotkey.clone();
        let rows = crate::platform::probe(&self.native.facts());
        let unsupported = rows
            .iter()
            .find(|r| r.feature == crate::platform::HUD_SNAP && r.state == "unsupported");
        let state = if chord.is_empty() {
            Hotkey::Disabled
        } else if let Some(row) = unsupported {
            Hotkey::Unsupported {
                reason: row.reason.clone(),
            }
        } else if let Err(why) = self.native.register_hotkey(&chord) {
            tracing::warn!(
                chord = chord.as_str(),
                "the HUD snap hotkey is taken: {why}"
            );
            Hotkey::Taken { chord }
        } else {
            Hotkey::Registered { chord }
        };
        *self
            .hud_snap
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(state.clone());
        state
    }

    /// The window underneath the HUD, unless the platform cannot say.
    fn window_below(&self) -> Option<crate::hud::WindowBelow> {
        let rows = crate::platform::probe(&self.native.facts());
        let unsupported = rows
            .iter()
            .any(|r| r.feature == crate::platform::HUD_BELOW && r.state == "unsupported");
        if unsupported {
            return None;
        }
        self.native.window_below()
    }

    /// Whether closing the last window keeps the app in the tray.
    #[must_use]
    pub fn keeps_running_in_tray(&self) -> bool {
        self.options.options.keep_in_tray && self.native.facts().sni_host != Some(false)
    }

    pub(crate) async fn feature(&self, window: &str, request: Request) -> Result<Value, String> {
        match request {
            Request::CheckUpdate => to_json(&self.updater.check(true).await),
            Request::BackgroundCheck => to_json(&self.updater.check(false).await),
            Request::ApplyUpdate => to_json(&self.updater.apply().await),
            Request::PluginPanels { profile } => self.plugin_panels(&profile).await,
            Request::CatalogEntry { kind, id, profile } => {
                self.catalog_entry(kind, &id, &profile).await
            }
            Request::CatalogInstall { kind, id, profile } => {
                self.catalog_install(kind, &id, &profile).await
            }
            other => self.feature_now(window, other),
        }
    }

    /// The requests that answer at once.
    fn feature_now(&self, window: &str, request: Request) -> Result<Value, String> {
        match request {
            Request::PlatformSupport => to_json(&json!({
                "features": crate::platform::probe(&self.native.facts()),
            })),
            Request::LaunchOptions => Ok(self.launch_options(window)),
            Request::InstallCli => Ok(self.install_cli()),
            Request::RestartApp => self.native.restart().map(|()| ok()),
            Request::TakeLinks => {
                let mut links = self.links.lock().map_err(|e| e.to_string())?;
                links.ready = true;
                to_json(&std::mem::take(&mut links.pending))
            }
            Request::NotifyApproval(approval) => self.notify_approval(approval),
            Request::RequestSettled { request_id } => {
                self.notices.resolved(&request_id);
                Ok(ok())
            }
            Request::SetTray(state) => self.native.set_tray(&state.menu()).map(|()| ok()),
            Request::HudToggle {
                profile,
                session_id,
            } => self
                .native
                .toggle_hud(&format!("#hud={profile}/{session_id}"))
                .map(|shown| json!({"shown": shown})),
            Request::HudReset => self.native.reset_hud().map(|()| ok()),
            Request::HudDrag => self.native.drag_hud().map(|()| ok()),
            Request::WindowBelow => to_json(&self.window_below()),
            Request::ReadWindowBelow => to_json(&crate::below::report(
                &self.native.facts(),
                self.native.screen().as_ref(),
                std::process::id(),
            )),
            request => self.quick_ask(window, request),
        }
    }

    fn quick_ask(&self, window: &str, request: Request) -> Result<Value, String> {
        match request {
            Request::QuickAskTarget { profile } => {
                let reuse = Duration::from_secs(self.options.options.quick_ask.reuse_secs);
                let target = self.quick_asks.target(&profile, Instant::now(), reuse);
                Ok(json!({"session_id": target}))
            }
            Request::QuickAskUsed {
                profile,
                session_id,
            } => {
                self.quick_asks.used(&profile, &session_id, Instant::now());
                Ok(ok())
            }
            Request::OpenInMain {
                profile,
                session_id,
            } => {
                self.native.open_in_main(&profile, &session_id)?;
                self.native.hide_window(window).map(|()| ok())
            }
            Request::HidePanel => self.native.hide_window(window).map(|()| ok()),
            Request::NewWindow => self.native.open_window("").map(|()| ok()),
            Request::PopOut {
                profile,
                session_id,
            } => self
                .native
                .open_window(&format!("#session={profile}/{session_id}"))
                .map(|()| ok()),
            _ => Err("not a request of this kind".to_owned()),
        }
    }

    /// Links `~/.local/bin/prometheus` to this install's CLI.
    fn install_cli(&self) -> Value {
        let Some(target) = self.native.cli_link_target() else {
            return json!({"ok": false, "reason": "this install puts the command-line tool on PATH itself"});
        };
        let path = std::env::var_os("PATH");
        match crate::cli_link::install(&self.config.user_home, &target, path.as_deref()) {
            Ok(installed) => json!({"ok": true, "installed": installed}),
            Err(reason) => json!({"ok": false, "reason": reason}),
        }
    }

    fn launch_options(&self, window: &str) -> Value {
        let o = &self.options.options;
        let hotkey = self
            .hotkey
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        json!({
            // The calling window's label: the page keeps its drafts per window.
            "window": window,
            "notices": self.options.notices,
            "keep_in_tray": o.keep_in_tray,
            // Whether Settings offers "Install command-line tool".
            "cli_link": self.native.cli_link_target().is_some(),
            "hotkey": hotkey,
            "quick_ask": {
                "hotkey": o.quick_ask.hotkey,
                "profile": o.quick_ask.profile,
                "reuse_secs": o.quick_ask.reuse_secs,
            },
            "hud": {
                "snap_hotkey": o.hud.snap_hotkey,
                "tap_to_summon": o.hud.tap_to_summon,
                "snap": self.hud_snap.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone(),
            },
        })
    }

    /// Posts the notification of an approval for an unfocused window. It
    /// has no actions: clicking it activates the app, which hands the
    /// window this request to focus (and nothing else).
    fn notify_approval(&self, approval: Approval) -> Result<Value, String> {
        let (title, body) = approval.notification();
        self.native.notify(&title, &body)?;
        self.notices.posted(approval);
        Ok(ok())
    }

    /// Runs the bundled binary with `args` (after `--json`) and parses the
    /// JSON it prints; an error document becomes its message.
    pub(crate) async fn bundled_json(
        &self,
        args: &[&str],
        bound: Duration,
    ) -> Result<Value, String> {
        let stdout = self.run_bundled(args, bound).await?;
        serde_json::from_slice(&stdout)
            .map_err(|e| format!("the bundled prometheus binary printed no JSON: {e}"))
    }

    /// Runs the bundled binary with `--json` and `args`; its stdout.
    pub(crate) async fn run_bundled(
        &self,
        args: &[&str],
        bound: Duration,
    ) -> Result<Vec<u8>, String> {
        let run = tokio::process::Command::new(&self.config.binary)
            .arg("--json")
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let out = tokio::time::timeout(bound, run)
            .await
            .map_err(|_| "the bundled prometheus binary timed out".to_owned())?
            .map_err(|e| format!("could not run the bundled prometheus binary: {e}"))?;
        if out.status.success() {
            return Ok(out.stdout);
        }
        let error: Option<Value> = serde_json::from_slice(&out.stderr).ok();
        Err(error
            .as_ref()
            .and_then(|e| e["error"]["message"].as_str())
            .map_or_else(
                || String::from_utf8_lossy(&out.stderr).trim().to_owned(),
                str::to_owned,
            ))
    }

    async fn plugin_panels(&self, profile: &str) -> Result<Value, String> {
        let printed = self
            .bundled_json(&["-p", profile, "plugins", "panels"], QUERY_BOUND)
            .await?;
        let listing: Listing = serde_json::from_value(printed).map_err(|e| e.to_string())?;
        self.panels.set(profile, &listing);
        to_json(&listing)
    }

    async fn catalog_entry(&self, kind: Kind, id: &str, profile: &str) -> Result<Value, String> {
        if !catalog::is_catalog_id(kind, id) {
            return Err(catalog::unknown(id));
        }
        let printed = self
            .bundled_json(
                &["-p", profile, "desktop", "catalog-entry", kind.as_str(), id],
                QUERY_BOUND,
            )
            .await
            .map_err(|_| catalog::unknown(id))?;
        let entry: Entry = serde_json::from_value(printed).map_err(|_| catalog::unknown(id))?;
        to_json(&entry)
    }

    async fn catalog_install(&self, kind: Kind, id: &str, profile: &str) -> Result<Value, String> {
        if !catalog::is_catalog_id(kind, id) {
            return Err(catalog::unknown(id));
        }
        let args: Vec<&str> = match kind {
            Kind::Plugin => vec![
                "-p", profile, "plugins", "install", id, "--enable", "--grant", "all",
            ],
            Kind::Skill => vec!["-p", profile, "skills", "install", id],
        };
        self.run_bundled(&args, INSTALL_BOUND).await?;
        Ok(json!({"ok": true, "installed": id}))
    }
}
