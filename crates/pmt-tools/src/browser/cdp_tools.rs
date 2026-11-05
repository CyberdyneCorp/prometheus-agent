//! `browser_cdp` and `browser_dialog` (spec: execution-environments,
//! `browser_cdp` and `browser_dialog` tools).
//!
//! Both are offered only while an explicit CDP endpoint is configured
//! (`/browser connect` or `browser.cdp_url`) and act on the calling
//! session's connected browser. `browser_cdp` sends one raw DevTools
//! command: page-level domains need the `target_id` they go to (attached
//! as a flattened session once), a `frame_id` of an out-of-process frame
//! goes through that frame's own session, and commands that would close or
//! crash the browser, close a target this session did not open, or open a
//! URL the browser URL safety refuses are refused. `browser_dialog`
//! answers the oldest pending JavaScript dialog (or the one named).

use std::sync::Arc;
use std::time::Duration;

use pmt_core::capability::Capability;
use serde_json::{Value, json};

use super::cdp::Cdp;
use super::session::{BrowserSession, Origin};
use super::{TOOLSET, exec::BrowserExec};
use crate::descriptor::{
    CallContext, HandlerFuture, Owner, RegisteredTool, ResourceClaims, ToolDescriptor, ToolHandler,
};
use crate::environments::{Environments, part};
use crate::probe::FnProbe;
use crate::result::ToolResult;

/// Domains whose commands act on one page target.
const PAGE_DOMAINS: &[&str] = &["Page", "Runtime", "DOM", "Emulation", "Input", "Network"];
/// Methods never sent.
const REFUSED: &[&str] = &["Browser.close", "Browser.crash"];
/// `timeout_secs` default and maximum.
const TIMEOUT: (u64, u64) = (30, 300);

/// One `browser_cdp` call.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CdpCall {
    /// `Domain.method`.
    pub method: String,
    /// Its parameters (an object).
    pub params: Value,
    /// The page target page-level domains go to.
    pub target_id: Option<String>,
    /// An out-of-process frame the command goes to.
    pub frame_id: Option<String>,
    /// Bound on the command (default 30, at most 300).
    pub timeout_secs: Option<u64>,
}

/// Whether `key` names a URL or a list of them (`url`, `frameUrl`,
/// `documentURL`, `urls`...).
fn url_key(key: &str) -> bool {
    let key = key.strip_suffix('s').unwrap_or(key);
    key == "url" || key.ends_with("Url") || key.ends_with("URL")
}

/// Every string under a URL-named key of `params`, at any depth: the URLs
/// browser URL safety checks before the command is sent.
fn url_params(params: &Value) -> Vec<&str> {
    let mut urls = Vec::new();
    let mut stack = vec![(false, params)];
    while let Some((under_url_key, value)) = stack.pop() {
        match value {
            Value::String(text) if under_url_key => urls.push(text.as_str()),
            Value::Array(items) => stack.extend(items.iter().map(|v| (under_url_key, v))),
            Value::Object(map) => stack.extend(map.iter().map(|(k, v)| (url_key(k), v))),
            _ => {}
        }
    }
    urls
}

/// Identifies a connection object (a reconnect makes a new one).
fn generation(cdp: &Arc<Cdp>) -> usize {
    Arc::as_ptr(cdp) as usize
}

impl BrowserSession {
    /// Runs one `browser_cdp` call.
    ///
    /// # Errors
    ///
    /// The refusal, a missing `target_id`, an unknown frame, or the
    /// browser's error.
    pub async fn cdp_call(&self, call: &CdpCall) -> Result<Value, String> {
        let method = call.method.as_str();
        if REFUSED.contains(&method) {
            return Err(format!("{method} is not allowed"));
        }
        if method == "Target.closeTarget" {
            let target = call.params["targetId"].as_str().unwrap_or_default();
            if !self.opened(target).await {
                return Err(
                    "Target.closeTarget is allowed only for targets this session opened".to_owned(),
                );
            }
        }
        for url in url_params(&call.params) {
            self.safety.check(url).await?;
        }
        let timeout =
            Duration::from_secs(call.timeout_secs.unwrap_or(TIMEOUT.0).clamp(1, TIMEOUT.1));
        let cdp = self.cdp();
        let domain = method.split('.').next().unwrap_or_default();
        let session = match (&call.frame_id, &call.target_id) {
            (Some(frame), target) => {
                Some(self.frame_session(&cdp, frame, target.as_deref()).await?)
            }
            (None, Some(target)) => Some(self.target_session(&cdp, target).await?),
            (None, None) if PAGE_DOMAINS.contains(&domain) => {
                return Err(format!(
                    "{domain} commands act on one page: pass target_id (Target.getTargets lists them)"
                ));
            }
            (None, None) => None,
        };
        let reply = cdp
            .call_within(method, call.params.clone(), session.as_deref(), timeout)
            .await?;
        if method == "Target.createTarget"
            && let Some(id) = reply.get("targetId").and_then(Value::as_str)
        {
            self.tabs.lock().await.record_opened(id);
        }
        Ok(reply)
    }

    async fn opened(&self, target: &str) -> bool {
        self.tabs.lock().await.was_opened(target)
    }

    /// The flattened session of `target`, attached once per connection
    /// (with page events on, so its dialogs are seen).
    async fn target_session(&self, cdp: &Arc<Cdp>, target: &str) -> Result<String, String> {
        let mut attached = self.attached.lock().await;
        if let Some((session, generation_of)) = attached.get(target)
            && *generation_of == generation(cdp)
        {
            return Ok(session.clone());
        }
        let reply = cdp
            .call(
                "Target.attachToTarget",
                json!({"targetId": target, "flatten": true}),
                None,
            )
            .await
            .map_err(|why| format!("cannot attach to target {target}: {why}"))?;
        let session = reply["sessionId"]
            .as_str()
            .ok_or("the browser's reply has no sessionId")?
            .to_owned();
        let _ = cdp.call("Page.enable", json!({}), Some(&session)).await;
        attached.insert(target.to_owned(), (session.clone(), generation(cdp)));
        Ok(session)
    }

    /// The session of frame `frame`: its own target when it runs out of
    /// process (an `iframe` target whose id is the frame id), else the
    /// page `target` it lives in.
    async fn frame_session(
        &self,
        cdp: &Arc<Cdp>,
        frame: &str,
        target: Option<&str>,
    ) -> Result<String, String> {
        let targets = cdp.call("Target.getTargets", json!({}), None).await?;
        let out_of_process = targets["targetInfos"].as_array().is_some_and(|list| {
            list.iter()
                .any(|t| t["type"] == "iframe" && t["targetId"] == frame)
        });
        if out_of_process {
            return self.target_session(cdp, frame).await;
        }
        match target {
            Some(target) => self.target_session(cdp, target).await,
            None => Err(format!(
                "frame {frame} is not an out-of-process frame: pass the target_id of its page"
            )),
        }
    }

    /// Answers dialog `id` (the oldest when `None`).
    ///
    /// # Errors
    ///
    /// `no pending dialog`, or the browser's error.
    pub async fn answer_dialog(
        &self,
        id: Option<u64>,
        accept: bool,
        prompt_text: Option<&str>,
    ) -> Result<(), String> {
        self.dialogs()
            .respond(&self.cdp(), id, accept, prompt_text)
            .await
    }
}

/// `browser_cdp` and `browser_dialog`, offered while an explicit endpoint
/// is configured.
#[must_use]
pub fn tools(environments: &Arc<Environments>) -> Vec<RegisteredTool> {
    let probe = Arc::new(FnProbe::new("browser-cdp", |context| {
        let configured = context
            .config
            .browser
            .cdp_url
            .as_deref()
            .is_some_and(|url| !url.trim().is_empty());
        if configured || super::connect::any_connected() {
            Ok(())
        } else {
            Err("no browser connected (/browser connect or browser.cdp_url)".to_owned())
        }
    }));
    vec![
        RegisteredTool::new(
            cdp_descriptor(),
            Owner::Builtin,
            Arc::new(CdpHandler(Arc::clone(environments))),
        )
        .with_probe(Arc::clone(&probe) as _),
        RegisteredTool::new(
            dialog_descriptor(),
            Owner::Builtin,
            Arc::new(DialogHandler(Arc::clone(environments))),
        )
        .with_probe(probe),
    ]
}

/// The calling session's connected browser.
async fn connected(exec: &BrowserExec, session_id: &str) -> Result<Arc<BrowserSession>, String> {
    let key = format!("{session_id}/default");
    if exec.manager().explicit_endpoint(&key).is_none() {
        return Err(
            "no browser is connected: run /browser connect, or set browser.cdp_url".to_owned(),
        );
    }
    let browser = exec.browser(&key).await?;
    match browser.origin() {
        Origin::Connected(_) => Ok(browser),
        _ => Err("the session's browser is not the connected one".to_owned()),
    }
}

struct CdpHandler(Arc<Environments>);

impl ToolHandler for CdpHandler {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let exec = part(&self.0, context, "browser", |env| env.browser.clone())?;
            let text = |key: &str| arguments[key].as_str().map(str::to_owned);
            let call = CdpCall {
                method: text("method").unwrap_or_default(),
                params: arguments
                    .get("params")
                    .cloned()
                    .filter(Value::is_object)
                    .unwrap_or_else(|| json!({})),
                target_id: text("target_id"),
                frame_id: text("frame_id"),
                timeout_secs: arguments["timeout_secs"].as_u64(),
            };
            let outcome = match connected(&exec, &context.session_id).await {
                Ok(browser) => browser.cdp_call(&call).await.map(|reply| {
                    let pending = browser.dialogs().pending();
                    if pending.is_empty() {
                        json!({"result": reply})
                    } else {
                        json!({"result": reply, "pending_dialogs": pending})
                    }
                }),
                Err(why) => Err(why),
            };
            Ok(match outcome {
                Ok(body) => ToolResult::ok(body.to_string()),
                Err(why) => ToolResult::error(format!("browser_cdp: {why}")),
            })
        })
    }
}

struct DialogHandler(Arc<Environments>);

impl ToolHandler for DialogHandler {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let exec = part(&self.0, context, "browser", |env| env.browser.clone())?;
            let accept = match arguments["action"].as_str() {
                Some("accept") => true,
                Some("dismiss") => false,
                _ => {
                    return Ok(ToolResult::error(
                        "browser_dialog: action must be accept or dismiss",
                    ));
                }
            };
            let prompt = arguments["prompt_text"].as_str();
            let id = arguments["dialog_id"].as_u64();
            let outcome = match connected(&exec, &context.session_id).await {
                Ok(browser) => browser.answer_dialog(id, accept, prompt).await,
                Err(why) => Err(why),
            };
            Ok(match outcome {
                Ok(()) => ToolResult::ok(if accept { "accepted" } else { "dismissed" }),
                Err(why) if why == "no pending dialog" => ToolResult::ok(why),
                Err(why) => ToolResult::error(format!("browser_dialog: {why}")),
            })
        })
    }
}

fn common(mut d: ToolDescriptor) -> ToolDescriptor {
    d.capabilities = vec![Capability::parse("net.connect:*").expect("valid capability")];
    d.claims = Some(ResourceClaims::exclusive(TOOLSET));
    d.timeout = Some(Duration::from_secs(TIMEOUT.1 + 30));
    d
}

fn cdp_descriptor() -> ToolDescriptor {
    common(ToolDescriptor::new(
        "browser_cdp",
        TOOLSET,
        "Send one raw Chrome DevTools Protocol command to the connected browser. \
         Page-level domains (Page, Runtime, DOM, Emulation, Input, Network) need target_id \
         (list targets with Target.getTargets); frame_id sends to an out-of-process iframe. \
         Browser.close, Browser.crash and closing targets you did not open are refused.",
        json!({
            "type": "object",
            "properties": {
                "method": {"type": "string", "description": "Domain.method, e.g. Page.navigate."},
                "params": {"type": "object", "description": "The command's parameters."},
                "target_id": {"type": "string", "description": "Target the command goes to."},
                "frame_id": {"type": "string", "description": "Out-of-process frame the command goes to."},
                "timeout_secs": {"type": "integer", "minimum": 1, "maximum": TIMEOUT.1, "description": "Bound on the command (default 30)."}
            },
            "required": ["method"],
            "additionalProperties": false
        }),
    ))
}

fn dialog_descriptor() -> ToolDescriptor {
    common(ToolDescriptor::new(
        "browser_dialog",
        TOOLSET,
        "Answer a JavaScript dialog (alert, confirm, prompt, beforeunload) in the connected \
         browser: accept or dismiss the oldest pending one, or the one dialog_id names; \
         prompt_text answers a prompt().",
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["accept", "dismiss"]},
                "prompt_text": {"type": "string", "description": "Text for a prompt() dialog."},
                "dialog_id": {"type": "integer", "description": "The dialog (default: the oldest pending)."}
            },
            "required": ["action"],
            "additionalProperties": false
        }),
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::url_params;

    #[test]
    fn every_url_named_parameter_is_checked_at_any_depth() {
        let params = json!({
            "url": "https://a.example/",
            "frameUrl": "http://169.254.169.254/",
            "options": {"documentURL": "http://10.0.0.1/", "count": 2},
            "entries": [{"baseURL": "https://b.example/"}],
            "redirectUrls": ["http://127.0.0.1/"],
            "expression": "fetch('http://not-a-url-param/')",
            "urlFilter": "*"
        });
        let mut urls = url_params(&params);
        urls.sort_unstable();
        assert_eq!(
            urls,
            [
                "http://10.0.0.1/",
                "http://127.0.0.1/",
                "http://169.254.169.254/",
                "https://a.example/",
                "https://b.example/"
            ]
        );
    }
}
