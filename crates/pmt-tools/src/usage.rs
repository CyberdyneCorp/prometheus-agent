//! Metered tool calls (spec: web-and-media-tools, Media tool credentials
//! and billing are explicit; observability, Per-call usage ledger).
//!
//! A web or media tool puts the provider that served a call, its model
//! and its `billing_source` in the result's metadata; the handler then
//! hands the call to the session's [`UsageSink`], which appends the
//! ledger row (`purpose = tool.<name>`). Cached answers and `local`
//! providers bill nobody and are not recorded.

use std::sync::Arc;

use serde_json::Value;

use crate::descriptor::CallContext;
use crate::result::ToolResult;

/// One metered tool call for the usage ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUsage {
    /// The model's call id.
    pub call_id: String,
    /// The calling session.
    pub session_id: String,
    /// `tool.<name>`.
    pub purpose: String,
    /// The provider that served it.
    pub provider: String,
    /// Its model (empty when the provider has none).
    pub model: String,
    /// `subscription:<provider>[/<label>]`, `api_key:<provider>[/<label>]`,
    /// `keyless:<vendor>` or `local`.
    pub billing_source: String,
}

/// Where a session's metered tool calls are recorded.
pub type UsageSink = Arc<dyn Fn(ToolUsage) + Send + Sync>;

/// Result metadata key of the serving provider.
pub const PROVIDER: &str = "provider";
/// Result metadata key of the model.
pub const MODEL: &str = "model";
/// Result metadata key of the billing source.
pub const BILLING_SOURCE: &str = "billing_source";
/// Result metadata key set on answers served from a cache.
pub const CACHED: &str = "cached";

/// Marks `result` as served by `provider` (`model`) billed to `billing`.
pub fn mark(result: &mut ToolResult, provider: &str, model: &str, billing: &str) {
    let meta = &mut result.metadata;
    meta.insert(PROVIDER.into(), Value::from(provider));
    if !model.is_empty() {
        meta.insert(MODEL.into(), Value::from(model));
    }
    meta.insert(BILLING_SOURCE.into(), Value::from(billing));
}

/// Hands the call behind `result` to `sink` when it reached a provider
/// that bills (not cached, not `local`).
pub fn record(sink: Option<&UsageSink>, context: &CallContext, tool: &str, result: &ToolResult) {
    let Some(sink) = sink else { return };
    let meta = &result.metadata;
    let text = |key: &str| meta.get(key).and_then(Value::as_str).unwrap_or_default();
    let billing = text(BILLING_SOURCE);
    let cached = meta.get(CACHED).and_then(Value::as_bool) == Some(true);
    if billing.is_empty() || billing == "local" || cached {
        return;
    }
    sink(ToolUsage {
        call_id: context.call_id.clone(),
        session_id: context.session_id.clone(),
        purpose: format!("tool.{tool}"),
        provider: text(PROVIDER).to_owned(),
        model: text(MODEL).to_owned(),
        billing_source: billing.to_owned(),
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn billed_calls_are_recorded_and_cached_or_local_ones_are_not() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let into = Arc::clone(&seen);
        let sink: UsageSink = Arc::new(move |u| into.lock().unwrap().push(u));
        let context = CallContext {
            call_id: "c1".into(),
            session_id: "s1".into(),
            cancel: pmt_core::cancel::CancelToken::new(),
        };
        let mut result = ToolResult::ok("x");
        mark(&mut result, "fal", "fal-ai/flux-2/klein/9b", "api_key:fal");
        record(Some(&sink), &context, "image_generate", &result);
        let mut local = ToolResult::ok("x");
        mark(&mut local, "piper", "", "local");
        record(Some(&sink), &context, "text_to_speech", &local);
        result.metadata.insert(CACHED.into(), Value::Bool(true));
        record(Some(&sink), &context, "image_generate", &result);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].purpose, "tool.image_generate");
        assert_eq!(seen[0].billing_source, "api_key:fal");
    }
}
