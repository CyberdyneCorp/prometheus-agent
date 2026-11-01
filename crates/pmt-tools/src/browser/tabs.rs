//! The page helpers `browser_exec` code calls (spec: execution-environments,
//! `browser_exec` tool): tabs, navigation, script evaluation, input,
//! screenshots and raw CDP, on the session's current tab.
//!
//! Tabs are attached as flattened CDP sessions; the attachment belongs to
//! one connection, so a tab is re-attached after the supervisor
//! reconnects.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};

use super::cdp::Cdp;
use super::session::BrowserSession;

/// Domains whose commands go to the current tab, not the browser.
const PAGE_DOMAINS: &[&str] = &["Page", "Runtime", "DOM", "Emulation", "Input", "Network"];
/// Methods `cdp()` never sends.
const REFUSED: &[&str] = &["Browser.close", "Browser.crash"];
/// Characters of page text `page_info` returns.
const PAGE_TEXT_CHARS: usize = 4_000;

/// One attached tab.
#[derive(Debug, Clone)]
struct Tab {
    target_id: String,
    session_id: String,
    /// The connection the attachment belongs to.
    endpoint_generation: usize,
}

/// The tabs of a session.
#[derive(Debug, Default)]
pub struct Tabs {
    current: Option<Tab>,
    opened: Vec<String>,
}

/// Identifies a connection object (a reconnect makes a new one).
fn generation(cdp: &Arc<Cdp>) -> usize {
    Arc::as_ptr(cdp) as usize
}

impl BrowserSession {
    /// Opens a tab (on `url`, else blank) and makes it current.
    ///
    /// # Errors
    ///
    /// A refused URL or a protocol failure.
    pub async fn new_tab(&self, url: Option<&str>) -> Result<Value, String> {
        if let Some(url) = url {
            self.safety.check(url).await?;
        }
        let cdp = self.cdp();
        let created = cdp
            .call("Target.createTarget", json!({"url": "about:blank"}), None)
            .await?;
        let target_id = text(&created, "targetId")?;
        let tab = attach(&cdp, &target_id).await?;
        {
            let mut tabs = self.tabs.lock().await;
            tabs.opened.push(target_id.clone());
            tabs.current = Some(tab);
        }
        if let Some(url) = url {
            self.goto_url(url).await?;
        }
        Ok(json!(target_id))
    }

    /// Navigates the current tab.
    ///
    /// # Errors
    ///
    /// A refused URL, a navigation error or a protocol failure.
    pub async fn goto_url(&self, url: &str) -> Result<Value, String> {
        self.safety.check(url).await?;
        let (cdp, tab) = self.current().await?;
        let reply = cdp
            .call("Page.navigate", json!({"url": url}), Some(&tab.session_id))
            .await?;
        if let Some(error) = reply.get("errorText").and_then(Value::as_str) {
            return Err(format!("navigation to {url} failed: {error}"));
        }
        Ok(json!({"url": url}))
    }

    /// Waits until the current page has loaded (`document.readyState` is
    /// `complete`).
    ///
    /// # Errors
    ///
    /// Not loaded within `timeout`, or a protocol failure.
    pub async fn wait_for_load(&self, timeout: Duration) -> Result<Value, String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.evaluate("document.readyState").await? == json!("complete") {
                return Ok(json!(true));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "the page did not load within {} s",
                    timeout.as_secs()
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// The current page's URL, title and visible text, the open tabs and
    /// pending dialogs.
    ///
    /// # Errors
    ///
    /// A protocol failure.
    pub async fn page_info(&self) -> Result<Value, String> {
        let page = self
            .evaluate(&format!(
                "({{url: location.href, title: document.title, text: (document.body ? document.body.innerText : '').slice(0, {PAGE_TEXT_CHARS})}})"
            ))
            .await?;
        let targets = self
            .cdp()
            .call("Target.getTargets", json!({}), None)
            .await?;
        let tabs = targets["targetInfos"].as_array().map_or(0, |list| {
            list.iter().filter(|t| t["type"] == "page").count()
        });
        Ok(json!({
            "url": page["url"],
            "title": page["title"],
            "text": page["text"],
            "tabs": tabs,
            "pending_dialogs": self.dialogs().pending(),
        }))
    }

    /// Evaluates `expression` in the current tab and returns its value.
    ///
    /// # Errors
    ///
    /// The page's exception text or a protocol failure.
    pub async fn evaluate(&self, expression: &str) -> Result<Value, String> {
        let (cdp, tab) = self.current().await?;
        let reply = cdp
            .call(
                "Runtime.evaluate",
                json!({"expression": expression, "returnByValue": true, "awaitPromise": true}),
                Some(&tab.session_id),
            )
            .await?;
        if let Some(details) = reply.get("exceptionDetails") {
            let message = details
                .pointer("/exception/description")
                .or_else(|| details.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("script error");
            return Err(message.to_owned());
        }
        Ok(reply
            .pointer("/result/value")
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// Types `text` into the element matching `selector`.
    ///
    /// # Errors
    ///
    /// No such element, or a protocol failure.
    pub async fn fill_input(&self, selector: &str, text: &str) -> Result<Value, String> {
        let script = format!(
            "(() => {{ const el = document.querySelector({sel}); if (!el) throw new Error('no element matches ' + {sel}); \
             el.focus(); el.value = {value}; el.dispatchEvent(new Event('input', {{bubbles: true}})); \
             el.dispatchEvent(new Event('change', {{bubbles: true}})); return true; }})()",
            sel = json!(selector),
            value = json!(text)
        );
        self.evaluate(&script).await
    }

    /// Clicks at viewport position (`x`, `y`).
    ///
    /// # Errors
    ///
    /// A protocol failure.
    pub async fn click_at_xy(&self, x: f64, y: f64) -> Result<Value, String> {
        let (cdp, tab) = self.current().await?;
        for kind in ["mouseMoved", "mousePressed", "mouseReleased"] {
            cdp.call(
                "Input.dispatchMouseEvent",
                json!({"type": kind, "x": x, "y": y, "button": "left", "clickCount": 1}),
                Some(&tab.session_id),
            )
            .await?;
        }
        Ok(json!(true))
    }

    /// Saves a PNG screenshot of the current tab to `path` (relative to
    /// `workspace`) and returns the file path.
    ///
    /// # Errors
    ///
    /// A protocol or write failure.
    pub async fn capture_screenshot(
        &self,
        workspace: &Path,
        path: Option<&str>,
    ) -> Result<PathBuf, String> {
        let (cdp, tab) = self.current().await?;
        let shot = cdp
            .call(
                "Page.captureScreenshot",
                json!({"format": "png"}),
                Some(&tab.session_id),
            )
            .await?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(text(&shot, "data")?)
            .map_err(|err| format!("unreadable screenshot: {err}"))?;
        let name = path.map_or_else(
            || {
                format!(
                    "screenshot-{}.png",
                    ulid::Ulid::generate().to_string().to_lowercase()
                )
            },
            str::to_owned,
        );
        let inside = Path::new(&name)
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)));
        if !inside {
            return Err("screenshots are saved inside the workspace".to_owned());
        }
        let target = workspace.join(name);
        pmt_core::private_fs::write_atomic(&target, &bytes)
            .map_err(|err| format!("cannot save the screenshot: {err}"))?;
        Ok(target)
    }

    /// Sends a raw CDP command: page domains to the current tab, others to
    /// the browser. Closing the browser, crashing it or closing a target
    /// this session did not open is refused, and so is a `url` parameter
    /// the URL safety check refuses.
    ///
    /// # Errors
    ///
    /// The refusal or the protocol error.
    pub async fn raw_cdp(&self, method: &str, params: Value) -> Result<Value, String> {
        if REFUSED.contains(&method) {
            return Err(format!("{method} is not allowed"));
        }
        if method == "Target.closeTarget" {
            let target = params
                .get("targetId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !self.tabs.lock().await.opened.iter().any(|t| t == target) {
                return Err(
                    "Target.closeTarget is allowed only for tabs this session opened".to_owned(),
                );
            }
        }
        if let Some(url) = params.get("url").and_then(Value::as_str) {
            self.safety.check(url).await?;
        }
        let domain = method.split('.').next().unwrap_or_default();
        if PAGE_DOMAINS.contains(&domain) {
            let (cdp, tab) = self.current().await?;
            return cdp.call(method, params, Some(&tab.session_id)).await;
        }
        self.cdp().call(method, params, None).await
    }

    /// Makes sure a real page tab is current (an existing one, else a new
    /// blank tab) and returns its target id.
    ///
    /// # Errors
    ///
    /// A protocol failure.
    pub async fn ensure_real_tab(&self) -> Result<Value, String> {
        let (_, tab) = self.current().await?;
        Ok(json!(tab.target_id))
    }

    /// The current tab, attached on the current connection: re-attached
    /// after a reconnect, adopted from the open pages or created.
    async fn current(&self) -> Result<(Arc<Cdp>, Tab), String> {
        let cdp = self.cdp();
        let mut tabs = self.tabs.lock().await;
        let pages = page_targets(&cdp).await?;
        if let Some(tab) = &tabs.current
            && pages.contains(&tab.target_id)
        {
            if tab.endpoint_generation == generation(&cdp) {
                return Ok((cdp, tab.clone()));
            }
            let fresh = attach(&cdp, &tab.target_id).await?;
            tabs.current = Some(fresh.clone());
            return Ok((cdp, fresh));
        }
        let target_id = if let Some(target) = pages.first() {
            target.clone()
        } else {
            let created = cdp
                .call("Target.createTarget", json!({"url": "about:blank"}), None)
                .await?;
            let target = text(&created, "targetId")?;
            tabs.opened.push(target.clone());
            target
        };
        let tab = attach(&cdp, &target_id).await?;
        tabs.current = Some(tab.clone());
        Ok((cdp, tab))
    }
}

/// Attaches to `target_id` as a flattened session with page events on.
async fn attach(cdp: &Arc<Cdp>, target_id: &str) -> Result<Tab, String> {
    let attached = cdp
        .call(
            "Target.attachToTarget",
            json!({"targetId": target_id, "flatten": true}),
            None,
        )
        .await?;
    let session_id = text(&attached, "sessionId")?;
    cdp.call("Page.enable", json!({}), Some(&session_id))
        .await?;
    Ok(Tab {
        target_id: target_id.to_owned(),
        session_id,
        endpoint_generation: generation(cdp),
    })
}

/// The ids of the browser's page targets.
async fn page_targets(cdp: &Cdp) -> Result<Vec<String>, String> {
    let targets = cdp.call("Target.getTargets", json!({}), None).await?;
    Ok(targets["targetInfos"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter(|t| t["type"] == "page")
                .filter_map(|t| t["targetId"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default())
}

fn text(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("the browser's reply has no {key}"))
}
