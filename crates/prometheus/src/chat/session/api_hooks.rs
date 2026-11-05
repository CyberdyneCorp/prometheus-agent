//! `pre_api_request`, `post_api_request` and `api_request_error` around
//! every main-model call of a session (spec: plugin-system, Typed hook
//! registry), on every surface: the session's model port is the one place
//! each of its calls passes, whatever route a `/model` switch picked.

use pmt_agent::turn::model::{ModelFailure, ModelReply};
use pmt_plugins::host::CallInfo;
use pmt_providers::request::Request;
use pmt_providers::resolve::Target;
use serde_json::{Map, Value, json};

use crate::plugins_host::ProfilePlugins;

/// The session's plugins and what their calls report as the session.
#[derive(Clone)]
pub struct ApiHooks {
    plugins: ProfilePlugins,
    call: CallInfo,
}

impl ApiHooks {
    /// The hooks of `plugins` for the session `call` describes.
    #[must_use]
    pub fn new(plugins: ProfilePlugins, call: CallInfo) -> Self {
        Self { plugins, call }
    }

    fn fields(&self, target: &Target, extra: Value) -> Map<String, Value> {
        let mut fields = Map::new();
        fields.insert(
            "session_id".to_owned(),
            self.call
                .session_id
                .clone()
                .map_or(Value::Null, Value::String),
        );
        fields.insert("provider".to_owned(), json!(target.provider));
        fields.insert("model".to_owned(), json!(target.model));
        if let Value::Object(extra) = extra {
            fields.extend(extra);
        }
        fields
    }

    /// `pre_api_request`: the request about to be sent.
    pub async fn before(&self, target: &Target, request: &Request) {
        let extra = json!({
            "message_count": request.messages.len(),
            "tool_count": request.tools.len(),
        });
        let fields = self.fields(target, extra);
        self.plugins
            .fire_observer(&self.call, "pre_api_request", fields)
            .await;
    }

    /// `post_api_request` or `api_request_error`: how the call ended.
    pub async fn after(&self, target: &Target, result: &Result<ModelReply, ModelFailure>) {
        let (hook, extra) = match result {
            Ok(reply) => (
                "post_api_request",
                json!({
                    "usage": reply.usage,
                    "tool_call_count": reply.tool_calls.len(),
                    "stop_reason": reply.stop,
                }),
            ),
            Err(failure) => (
                "api_request_error",
                json!({
                    "error": failure.error.message(),
                    "code": failure.error.code().code,
                    "class": failure.class,
                }),
            ),
        };
        let fields = self.fields(target, extra);
        self.plugins.fire_observer(&self.call, hook, fields).await;
    }
}
