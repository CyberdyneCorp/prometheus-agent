//! Window tools forwarded as `ui.<tool>` requests (spec: desktop-app,
//! Desktop UI toolset contract; Preview pane tool).

use std::sync::Arc;
use std::time::Duration;

use pmt_gateway::FeatureAnswer;
use pmt_net::DestinationPolicy;
use pmt_net::resolve::SystemLookup;
use pmt_rpc::contracts::{DESKTOP_UI_FEATURE, UiCall};
use pmt_tools::ToolResult;
use pmt_tools::browser::safety::UrlSafety;
use serde_json::Value;

use super::{UiPort, answer, check, fail};

/// Runs window tool `tool`: checked, then asked of the window.
pub(super) async fn call(port: &UiPort, tool: &str, args: Value) -> ToolResult {
    if let Err(refused) = check::refusal(tool, &args) {
        return fail(refused);
    }
    if tool == "desktop_preview"
        && args.get("action").and_then(Value::as_str) == Some("open")
        && let Some(url) = args.get("url").and_then(Value::as_str)
        && let Err(refused) = preview_url(port, url).await
    {
        return fail(refused);
    }
    let timeout = check::deadline(tool, &args);
    match ask(port, tool, args, timeout).await {
        Ok(value) => answer(&value),
        Err(unanswered) => unanswered.result(tool, timeout),
    }
}

/// Why the window gave no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Unanswered {
    /// No desktop window is attached to the session.
    NoWindow,
    /// The window did not answer before the deadline.
    TimedOut,
}

impl Unanswered {
    /// What the model gets for `tool`, which waited `timeout`.
    pub(super) fn result(self, tool: &str, timeout: Duration) -> ToolResult {
        match self {
            Self::NoWindow => pmt_agent::tools::desktop::no_window(tool),
            Self::TimedOut => fail(format!(
                "{tool} timed out: the desktop window did not answer within {} s",
                timeout.as_secs()
            )),
        }
    }
}

/// Asks the session's desktop window to run `tool`; its answer with
/// secrets redacted.
pub(super) async fn ask(
    port: &UiPort,
    tool: &str,
    arguments: Value,
    timeout: Duration,
) -> Result<Value, Unanswered> {
    let link = port.link().ok_or(Unanswered::NoWindow)?;
    let params = UiCall {
        session_id: port.session.to_string(),
        arguments,
        deadline_ms: None,
    };
    let params = serde_json::to_value(params).unwrap_or_default();
    match link
        .ask_feature(&format!("ui.{tool}"), params, timeout, DESKTOP_UI_FEATURE)
        .await
    {
        FeatureAnswer::NoClient => Err(Unanswered::NoWindow),
        FeatureAnswer::Unanswered => Err(Unanswered::TimedOut),
        FeatureAnswer::Answered(value) => Ok(port.redactor.redact_json(value)),
    }
}

/// Whether the preview may load `url`: a workspace file or a loopback
/// dev server always, a web address under the URL safety policy of the
/// web tools (spec: desktop-app, Preview pane tool).
///
/// # Errors
///
/// The refusal text.
pub(super) async fn preview_url(port: &UiPort, url: &str) -> Result<(), String> {
    let Ok(parsed) = url::Url::parse(url) else {
        // A workspace path.
        return Ok(());
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return if parsed.scheme() == "file" {
            Ok(())
        } else {
            Err(format!(
                "the preview opens http(s) pages and workspace files, not {}:",
                parsed.scheme()
            ))
        };
    }
    if is_loopback(&parsed) {
        return Ok(());
    }
    let egress = &port.scope.config().security.egress;
    let destinations = DestinationPolicy::new(egress.allow_private, &egress.fake_ip_ranges)
        .map_err(|e| e.message().to_owned())?;
    let safety = UrlSafety {
        destinations,
        lookup: Arc::new(SystemLookup),
        secrets: port.redactor.clone(),
        egress: None,
        session: None,
    };
    safety.check(url).await
}

fn is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}
