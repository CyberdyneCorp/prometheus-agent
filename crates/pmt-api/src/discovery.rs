//! Model listing and the discovery and readiness endpoints (spec:
//! api-server, Model listing and Open `WebUI` compatibility; Discovery and
//! readiness endpoints), and the Jobs REST passthrough (Jobs REST API).

use std::sync::Arc;

use serde_json::json;

use crate::Shared;
use crate::auth::Caller;
use crate::host::{Catalog, JobsCall};
use crate::http::{Reply, Req, from_error, json_reply, not_found};

/// `GET /v1/models`: the profile's model id and its route aliases.
pub(crate) fn models(shared: &Shared, caller: &Caller) -> Reply {
    let scope = match shared.scope(&caller.profile) {
        Ok(scope) => scope,
        Err(reply) => return reply,
    };
    let api = &scope.config().api_server;
    let created = jiff::Timestamp::now().as_second();
    let entry = |id: &str| json!({"id": id, "object": "model", "created": created, "owned_by": "prometheus"});
    let mut data = vec![entry(&api.model_id(&caller.profile))];
    data.extend(api.model_routes.keys().map(|alias| entry(alias)));
    json_reply(200, &json!({"object": "list", "data": data}))
}

/// `GET /v1/capabilities`.
pub(crate) fn capabilities(shared: &Shared, caller: &Caller) -> Reply {
    let scope = match shared.scope(&caller.profile) {
        Ok(scope) => scope,
        Err(reply) => return reply,
    };
    let api = &scope.config().api_server;
    json_reply(
        200,
        &json!({
            "object": "prometheus.api_server.capabilities",
            "model": api.model_id(&caller.profile),
            "profile": caller.profile,
            "auth": {"type": "bearer", "required": true},
            "features": {
                "chat_completions": true,
                "responses_api": true,
                "run_submission": true,
                "run_status": true,
                "run_events_sse": true,
                "run_stop": true,
                "run_approval": true,
                "run_clarify": true,
                "run_unknown_resolution": true,
                "reasoning_streaming": true,
                "tool_progress_events": api.tool_progress_events,
                "sessions_api": true,
                "jobs_api": true,
                "session_key_header": true,
            },
        }),
    )
}

/// `GET /v1/skills` and `GET /v1/toolsets`.
pub(crate) async fn catalog(shared: &Shared, caller: &Caller, what: Catalog) -> Reply {
    match shared.host.api_catalog(&caller.profile, what).await {
        Ok(data) => json_reply(200, &json!({"object": "list", "data": data})),
        Err(err) => from_error(&err),
    }
}

/// `GET /api/model/options`: configured providers and models with
/// capability hints and pricing.
pub(crate) async fn model_options(shared: &Shared, caller: &Caller) -> Reply {
    match shared
        .host
        .api_catalog(&caller.profile, Catalog::Models)
        .await
    {
        Ok(options) => json_reply(200, &options),
        Err(err) => from_error(&err),
    }
}

/// `GET /health/detailed`: bounded status and counts only.
pub(crate) async fn health(shared: &Shared, caller: &Caller) -> Reply {
    let mut checks = match shared.host.api_health(&caller.profile).await {
        Ok(checks) => checks,
        Err(err) => return from_error(&err),
    };
    checks["api_runs"] = json!({"status": "ok", "active": shared.limits.active(&caller.profile)});
    let degraded = checks
        .as_object()
        .is_some_and(|all| all.values().any(|c| c["status"] != "ok"));
    let status = if degraded { "degraded" } else { "ok" };
    json_reply(200, &json!({"status": status, "checks": checks}))
}

/// `/api/jobs[/{id}[/{action}]]`.
pub(crate) async fn jobs(shared: &Arc<Shared>, caller: &Caller, rest: &[&str], req: &Req) -> Reply {
    let (job, action) = match rest {
        [] | [""] => (None, None),
        [id] => (Some((*id).to_owned()), None),
        [id, action @ ("pause" | "resume" | "run")] => {
            (Some((*id).to_owned()), Some((*action).to_owned()))
        }
        _ => return not_found("the route"),
    };
    let body = match req.json() {
        Ok(body) => body,
        Err(reply) => return reply,
    };
    let call = JobsCall {
        method: req.method.as_str().to_owned(),
        job,
        action,
        body,
    };
    match shared.host.api_jobs(&caller.profile, call).await {
        Ok((status, body)) => json_reply(status, &body),
        Err(err) => from_error(&err),
    }
}
