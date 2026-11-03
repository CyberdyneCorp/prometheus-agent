//! A session's approvals as plugins see them (spec: plugin-system,
//! Approval and human-input observer hooks; Approval transports): the
//! `pre_approval_request` / `post_approval_response` observers, and the
//! approval transport `plugins.approval_transport` selects.

use std::sync::Arc;

use pmt_config::schema::TransportFallback;
use pmt_core::secret::Redactor;
use pmt_plugins::host::CallInfo;
use pmt_plugins::host::surfaces_world::ApprovalRequest as WitRequest;
use pmt_tools::approval::ApprovalRequest;
use pmt_tools::approval::hooks::{
    ApprovalHooks, TransportOutcome, TransportRequest, bound_answer, choice_name,
};
use pmt_tools::dispatch::SeamFuture;
use serde_json::{Map, Value, json};

use super::ProfilePlugins;

/// The plugin side of one session's approval gate.
pub struct PluginApprovals {
    plugins: ProfilePlugins,
    call: CallInfo,
    redactor: Redactor,
    /// `plugins.approval_transport` (`None` when empty).
    selected: Option<String>,
    fallback: TransportFallback,
}

impl std::fmt::Debug for PluginApprovals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginApprovals")
            .field("selected", &self.selected)
            .finish_non_exhaustive()
    }
}

impl PluginApprovals {
    /// The approvals of the session `call` names.
    #[must_use]
    pub fn new(
        plugins: ProfilePlugins,
        call: CallInfo,
        redactor: Redactor,
        config: &pmt_config::schema::PluginsConfig,
    ) -> Self {
        Self {
            plugins,
            call,
            redactor,
            selected: (!config.approval_transport.is_empty())
                .then(|| config.approval_transport.clone()),
            fallback: config.approval_transport_fallback,
        }
    }

    fn fields(&self, request: &ApprovalRequest) -> Map<String, Value> {
        let command = request
            .subject
            .as_deref()
            .map(|s| self.redactor.redact_all(s));
        let fields = json!({
            "command": command,
            "description": self.redactor.redact_all(&request.summary()),
            "pattern_keys": request.remember.iter().collect::<Vec<_>>(),
            "session_key": self.call.session_id,
            "surface": self.call.surface,
            "tool": request.tool,
        });
        match fields {
            Value::Object(map) => map,
            _ => Map::new(),
        }
    }

    /// A failure that the fallback may route to the surface.
    fn failed(&self, why: String) -> TransportOutcome {
        tracing::warn!(transport = ?self.selected, "approval transport failed: {why}");
        match self.fallback {
            TransportFallback::Builtin => TransportOutcome::Builtin,
            TransportFallback::Deny => TransportOutcome::Denied(why),
        }
    }

    async fn present(&self, selected: &str, request: TransportRequest) -> TransportOutcome {
        let loaded = self.plugins.current();
        let Some(transport) = loaded.surfaces.transport(selected).cloned() else {
            return self.failed(format!("approval transport '{selected}' is not registered"));
        };
        let wit = WitRequest {
            id: request.id.clone(),
            digest: request.digest.clone(),
            command: request
                .request
                .subject
                .as_deref()
                .map(|s| self.redactor.redact_all(s))
                .unwrap_or_default(),
            description: self.redactor.redact_all(&request.request.summary()),
            surface: self.call.surface.clone(),
            timeout_secs: u32::try_from(request.timeout.as_secs()).unwrap_or(u32::MAX),
            choices: request
                .choices
                .iter()
                .map(|c| choice_name(*c).to_owned())
                .collect(),
        };
        let call = self.call.clone();
        let bound = request.timeout;
        let answer = tokio::task::spawn_blocking(move || {
            transport
                .instance
                .present_approval(&call, &transport.name, &wit, bound)
        })
        .await;
        let response = match answer {
            Ok(Ok(Ok(response))) => response,
            Ok(Ok(Err(why))) => return self.failed(why),
            Ok(Err(err)) => return self.failed(err.message().to_owned()),
            Err(join) => return self.failed(join.to_string()),
        };
        if response.id != request.id || response.digest != request.digest {
            return self.failed("the transport answered a stale or unbound request".to_owned());
        }
        // A choice the request did not offer is never routed elsewhere: the
        // transport tried to widen the request, so it is denied.
        bound_answer(&request, &response.id, &response.digest, &response.choice)
    }
}

impl ApprovalHooks for PluginApprovals {
    fn before<'a>(&'a self, request: &'a ApprovalRequest) -> SeamFuture<'a, ()> {
        Box::pin(async move {
            self.plugins
                .fire_observer(&self.call, "pre_approval_request", self.fields(request))
                .await;
        })
    }

    fn after<'a>(
        &'a self,
        request: &'a ApprovalRequest,
        choice: &'a str,
        decided_by: &'a str,
    ) -> SeamFuture<'a, ()> {
        Box::pin(async move {
            let mut fields = self.fields(request);
            fields.insert("choice".to_owned(), choice.into());
            fields.insert("decided_by".to_owned(), decided_by.into());
            self.plugins
                .fire_observer(&self.call, "post_approval_response", fields)
                .await;
        })
    }

    fn transport(&self, request: TransportRequest) -> SeamFuture<'_, Option<TransportOutcome>> {
        Box::pin(async move {
            let selected = self.selected.clone()?;
            Some(self.present(&selected, request).await)
        })
    }
}

/// The clarify observer of a session.
#[must_use]
pub fn input_observer(
    plugins: ProfilePlugins,
    call: CallInfo,
) -> pmt_agent::tools::clarify::InputObserver {
    Arc::new(move |hook, payload| {
        let mut fields = match payload {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        fields.insert("session_id".to_owned(), json!(call.session_id));
        fields.insert("surface".to_owned(), json!(call.surface));
        plugins.observe(call.clone(), hook, fields);
    })
}
