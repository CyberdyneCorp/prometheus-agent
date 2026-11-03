//! `transform_api_error_classification` (spec: plugin-system): every
//! callback sees each failed provider call before the built-in retry and
//! fallback policy applies, and the first valid answer in registration
//! order decides. A valid answer is a `modify` JSON object
//! `{reason, retryable?, should_compress?, should_rotate_credential?,
//! should_fallback?, message?}` whose `reason` is a known failover reason;
//! anything else is skipped with a warning.

use std::sync::Arc;

use pmt_providers::caller::{ErrorClassifier, FailureInfo};
use pmt_providers::classify::{ErrorClass, Reclassified, reason_class};
use pmt_providers::resolve::BoxFuture;
use serde::Deserialize;
use serde_json::Value;

use super::{Answer, HookSource, Payload};
use crate::host::CallInfo;
use crate::host::hook_world::Reply;

const HOOK: &str = "transform_api_error_classification";

/// The provider caller's classification seam over a profile's hooks.
pub struct PluginErrorClassifier {
    source: Arc<dyn HookSource>,
    call: CallInfo,
}

impl PluginErrorClassifier {
    /// Hooks read from `source` at each failure, for the session `call`.
    #[must_use]
    pub fn new(source: Arc<dyn HookSource>, call: CallInfo) -> Self {
        Self { source, call }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Answered {
    reason: String,
    retryable: Option<bool>,
    #[serde(default)]
    should_compress: bool,
    #[serde(default)]
    should_rotate_credential: bool,
    should_fallback: Option<bool>,
    message: Option<String>,
}

/// The first valid decision among `answers`.
#[must_use]
pub fn decide(answers: &[Answer]) -> Option<(Reclassified, ErrorClass)> {
    answers.iter().find_map(|answer| {
        let Reply::Modify(text) = &answer.reply else {
            return None;
        };
        let parsed: Answered = match serde_json::from_str(text) {
            Ok(parsed) => parsed,
            Err(err) => {
                tracing::warn!(plugin = %answer.plugin, "{HOOK} answer skipped: {err}");
                return None;
            }
        };
        let Some(class) = reason_class(&parsed.reason) else {
            tracing::warn!(plugin = %answer.plugin, "{HOOK} answer skipped: unknown failover reason '{}'", parsed.reason);
            return None;
        };
        Some((
            Reclassified {
                reason: parsed.reason,
                retryable: parsed.retryable,
                should_compress: parsed.should_compress,
                should_rotate_credential: parsed.should_rotate_credential,
                should_fallback: parsed.should_fallback,
                message: parsed.message,
            },
            class,
        ))
    })
}

impl ErrorClassifier for PluginErrorClassifier {
    fn classify<'a>(
        &'a self,
        failure: &'a FailureInfo,
    ) -> BoxFuture<'a, Option<(Reclassified, ErrorClass)>> {
        Box::pin(async move {
            let hooks = self.source.hooks();
            if !hooks.handles(HOOK) {
                return None;
            }
            let Ok(Value::Object(fields)) = serde_json::to_value(failure) else {
                return None;
            };
            let answers = hooks.fire(HOOK, &Payload::new(fields), &self.call).await;
            decide(&answers)
        })
    }
}
