//! Plugin UI panels (spec: desktop-app, Plugin UI contributions are
//! isolated). The shell learns the profile's panels from the bundled
//! binary (`prometheus --json plugins panels`), and serves each panel's
//! document on the plugin's own origin: `http://plugin-<id>.localhost` on
//! Windows, `plugin-<id>://localhost` elsewhere (a URI scheme per plugin),
//! so the webview gives every plugin its own storage. The document gets
//! the plugin UI SDK, whose only channel is `postMessage` to the window;
//! its policy forbids network requests, so the SDK's capabilities are all
//! a panel can reach.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// The largest panel document served.
pub const MAX_DOCUMENT: u64 = 1024 * 1024;

/// The policy of a panel document: scripts and styles of its own, no
/// network, no plugins, embeddable only by the app.
pub const POLICY: &str = "default-src 'none'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'none'; form-action 'none'; base-uri 'none'";

/// The plugin UI SDK, injected at the top of every panel document:
/// `window.prometheus` (also `window.sdk`) with `register()`, `context()`,
/// `sessions.list()` and `host.{connections, agents, ensureAgent,
/// warmAgent}()` (the connection registry and the agents across
/// connections); each call is a message to the window, which answers it
/// or rejects it.
pub const SDK: &str = r#"<script>(()=>{const p=new Map();let n=1;addEventListener("message",e=>{if(e.source!==parent)return;const m=e.data;if(!m||m.prometheus!==1||!p.has(m.id))return;const r=p.get(m.id);p.delete(m.id);m.error?r[1](new Error(m.error)):r[0](m.result)});const c=(method,params)=>new Promise((ok,no)=>{const id=n++;p.set(id,[ok,no]);parent.postMessage({prometheus:1,id,method,params:params||{}},"*")});const sdk=Object.freeze({register:()=>c("register"),context:()=>c("context"),sessions:Object.freeze({list:()=>c("sessions.list")}),host:Object.freeze({connections:()=>c("host.connections"),agents:()=>c("host.agents"),ensureAgent:(connection_id,profile)=>c("host.ensureAgent",{connection_id,profile}),warmAgent:(connection_id,profile)=>c("host.warmAgent",{connection_id,profile})})});Object.defineProperty(window,"prometheus",{value:sdk});Object.defineProperty(window,"sdk",{value:sdk})})();</script>"#;

/// One panel, as `plugins panels` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Panel {
    /// The profile whose plugin it is.
    pub profile: String,
    /// The plugin id.
    pub id: String,
    /// Its label.
    pub label: String,
    /// Its document.
    #[serde(skip_serializing)]
    pub entry: PathBuf,
    /// The SDK capabilities granted to it.
    pub sdk: Vec<String>,
}

/// A declared panel that does not load, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skipped {
    /// The profile.
    pub profile: String,
    /// The plugin id.
    pub id: String,
    /// Why.
    pub reason: String,
}

/// What `plugins panels` printed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    /// The panels that load.
    pub panels: Vec<Panel>,
    /// The ones that do not.
    #[serde(default)]
    pub skipped: Vec<Skipped>,
}

/// The URI scheme of plugin `id`'s panel.
#[must_use]
pub fn scheme(id: &str) -> String {
    format!("plugin-{id}")
}

/// The panels the shell serves, by plugin id (the latest listing of each
/// profile).
#[derive(Debug, Default)]
pub struct Panels {
    known: Mutex<BTreeMap<String, Panel>>,
}

impl Panels {
    /// Records `listing` of `profile`, replacing that profile's panels.
    pub fn set(&self, profile: &str, listing: &Listing) {
        let mut known = self.lock();
        known.retain(|_, p| p.profile != profile);
        for panel in &listing.panels {
            known.insert(panel.id.clone(), panel.clone());
        }
    }

    /// The document of plugin `id`'s panel, with the SDK.
    ///
    /// # Errors
    ///
    /// No such panel, or its document is missing or too large.
    pub fn document(&self, id: &str) -> Result<String, String> {
        let entry = self
            .lock()
            .get(id)
            .map(|p| p.entry.clone())
            .ok_or_else(|| format!("no panel of plugin {id}"))?;
        let len = std::fs::metadata(&entry)
            .map_err(|e| format!("{}: {e}", entry.display()))?
            .len();
        if len > MAX_DOCUMENT {
            return Err(format!("{} is larger than 1 MiB", entry.display()));
        }
        let text =
            std::fs::read_to_string(&entry).map_err(|e| format!("{}: {e}", entry.display()))?;
        Ok(with_sdk(&text))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Panel>> {
        self.known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// `document` with the SDK as the first script it runs.
#[must_use]
pub fn with_sdk(document: &str) -> String {
    let lower = document.to_ascii_lowercase();
    let at = lower
        .find("<head>")
        .map(|i| i + "<head>".len())
        .or_else(|| lower.find("<html>").map(|i| i + "<html>".len()))
        .unwrap_or(0);
    format!("{}{SDK}{}", &document[..at], &document[at..])
}

#[cfg(test)]
mod tests {
    use super::{Listing, Panel, Panels, SDK, with_sdk};

    #[test]
    fn the_sdk_runs_before_the_documents_own_scripts() {
        let doc = with_sdk("<!doctype html><html><head><script>x()</script></head></html>");
        assert!(doc.find(SDK).unwrap() < doc.find("x()").unwrap());
        assert!(with_sdk("<p>bare</p>").starts_with(SDK));
    }

    #[test]
    fn a_profile_listing_replaces_only_its_own_panels() {
        let dir = tempfile::tempdir().unwrap();
        let entry = dir.path().join("index.html");
        std::fs::write(&entry, "<html><head></head><body>a</body></html>").unwrap();
        let panel = |profile: &str, id: &str| Panel {
            profile: profile.into(),
            id: id.into(),
            label: id.into(),
            entry: entry.clone(),
            sdk: vec![],
        };
        let panels = Panels::default();
        panels.set(
            "default",
            &Listing {
                panels: vec![panel("default", "a")],
                skipped: vec![],
            },
        );
        panels.set(
            "work",
            &Listing {
                panels: vec![panel("work", "b")],
                skipped: vec![],
            },
        );
        panels.set("default", &Listing::default());
        assert!(panels.document("a").is_err());
        assert!(panels.document("b").unwrap().contains(SDK));
    }
}
