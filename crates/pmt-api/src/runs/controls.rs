//! Run controls (spec: api-server, Run controls): `stop`, `approval`,
//! `clarify`, `steer` and `resolve-unknown`. Bodies are exact (an extra or
//! missing field is 400 `invalid_params`); an answer names the pending
//! request and the `execution_generation` it was asked under, so a stale
//! or foreign answer is refused and the request stays pending. Answers go
//! through the hub, settling the same request other clients see.

use std::sync::Arc;

use pmt_gateway::hub::Answered;
use pmt_store::api::runs::{ApiRun, RunUpdate};
use serde_json::{Map, Value, json};

use super::{Run, draining};
use crate::Shared;
use crate::auth::Caller;
use crate::http::{Reply, Req, bad_request, conflict, from_error, json_reply, not_found};

/// `POST /v1/runs/{id}/<action>`.
pub(crate) async fn control(
    shared: &Arc<Shared>,
    caller: &Caller,
    id: &str,
    action: &str,
    req: &Req,
) -> Reply {
    if let Some(refused) = draining(shared) {
        return refused;
    }
    let store = match shared.store(&caller.profile) {
        Ok(store) => store,
        Err(reply) => return reply,
    };
    let run = match store.api_run(id) {
        Ok(Some(run)) => run,
        Ok(None) => return not_found(&format!("run {id}")),
        Err(err) => return from_error(&err),
    };
    let body = match req.json() {
        Ok(body) => body,
        Err(reply) => return reply,
    };
    let live = shared.runs.get(id).filter(|r| r.profile == caller.profile);
    let result = match action {
        "stop" => stop(shared, &run, live.as_deref()).await,
        "approval" => answer(shared, &run, live.as_deref(), &body, Kind::Approval),
        "clarify" => answer(shared, &run, live.as_deref(), &body, Kind::Clarify),
        "steer" => steer(shared, caller, &run, &body).await,
        "resolve-unknown" => resolve_unknown(shared, &store, &run, &body),
        _ => Err(not_found("the route")),
    };
    match result {
        Ok(reply) | Err(reply) => reply,
    }
}

/// The body's fields must be exactly `fields`.
fn exact<'a>(body: &'a Value, fields: &[&str]) -> Result<&'a Map<String, Value>, Reply> {
    let object = body.as_object().ok_or_else(invalid)?;
    let same = object.len() == fields.len() && fields.iter().all(|f| object.contains_key(*f));
    if same { Ok(object) } else { Err(invalid()) }
}

fn invalid() -> Reply {
    bad_request(
        "invalid_params",
        "the body must have exactly the documented fields",
    )
}

async fn stop(shared: &Shared, run: &ApiRun, live: Option<&Run>) -> Result<Reply, Reply> {
    if run.status == "unknown" {
        return Err(conflict(
            "unknown_execution",
            "the run's daemon stopped before it finished; resolve it with resolve-unknown",
        ));
    }
    if run.is_terminal() {
        return Ok(json_reply(200, &json!({"status": run.status})));
    }
    let Some(live) = live else {
        return Err(not_found(&format!("run {}", run.id)));
    };
    let queued = {
        let mut state = live.state.lock().expect("run poisoned");
        state.stopping = true;
        run.status == "queued"
    };
    live.record(&RunUpdate {
        status: Some("stopping".to_owned()),
        ..RunUpdate::default()
    });
    if !queued && let Err(err) = shared.host.interrupt(&live.profile, &live.session).await {
        tracing::debug!(run = run.id, "interrupt: {err}");
    }
    Ok(json_reply(200, &json!({"status": "stopping"})))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Approval,
    Clarify,
}

fn answer(
    shared: &Shared,
    run: &ApiRun,
    live: Option<&Run>,
    body: &Value,
    kind: Kind,
) -> Result<Reply, Reply> {
    let third = if kind == Kind::Approval {
        "choice"
    } else {
        "answer"
    };
    let fields = exact(body, &["request_id", "execution_generation", third])?;
    let request_id = fields["request_id"].as_str().ok_or_else(invalid)?;
    let generation = fields["execution_generation"]
        .as_i64()
        .ok_or_else(invalid)?;
    let value = match kind {
        Kind::Approval => {
            let choice = fields["choice"].as_str().ok_or_else(invalid)?;
            if !["once", "session", "always", "deny"].contains(&choice) {
                return Err(bad_request(
                    "invalid_params",
                    "choice must be once, session, always or deny",
                ));
            }
            json!({"decision": choice})
        }
        Kind::Clarify => match &fields["answer"] {
            Value::Array(answers) => json!({"answers": answers}),
            other => json!({"answers": [other]}),
        },
    };
    let kind_name = if kind == Kind::Approval {
        "approval"
    } else {
        "clarify"
    };
    let pending = live.and_then(|live| {
        live.state
            .lock()
            .expect("run poisoned")
            .pending
            .iter()
            .find(|p| p.control.id == request_id && p.control.kind == kind_name)
            .cloned()
    });
    let Some(pending) = pending else {
        return Err(conflict(
            "not_found",
            &format!("run {} has no pending {kind_name} {request_id}", run.id),
        ));
    };
    if pending.generation != generation {
        return Err(conflict(
            "stale_generation",
            &format!(
                "the request is pending under execution_generation {}",
                pending.generation
            ),
        ));
    }
    match shared.peer.answer(request_id, Some(value)) {
        Answered::Taken => Ok(json_reply(
            200,
            &json!({"status": "accepted", "request_id": request_id}),
        )),
        Answered::Settled | Answered::Unknown => {
            Err(conflict("not_found", "the request already settled"))
        }
    }
}

async fn steer(
    shared: &Shared,
    caller: &Caller,
    run: &ApiRun,
    body: &Value,
) -> Result<Reply, Reply> {
    let fields = exact(body, &["text"])?;
    let text = fields["text"]
        .as_str()
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(invalid)?;
    if run.is_terminal() || run.status == "unknown" {
        return Err(conflict(
            "not_running",
            &format!("run {} is {}", run.id, run.status),
        ));
    }
    let params = pmt_rpc::contracts::SteerParams {
        session_id: run.session_id.clone(),
        text: text.to_owned(),
        kind: pmt_rpc::contracts::SteerKind::default(),
    };
    shared
        .host
        .steer_session(&caller.profile, params)
        .await
        .map_err(|e| from_error(&e))?;
    Ok(json_reply(200, &json!({"status": "steered"})))
}

fn resolve_unknown(
    shared: &Shared,
    store: &pmt_store::Store,
    run: &ApiRun,
    body: &Value,
) -> Result<Reply, Reply> {
    let fields = exact(body, &["admission_id", "execution_generation"])?;
    let admission = fields["admission_id"].as_str().ok_or_else(invalid)?;
    let generation = fields["execution_generation"]
        .as_i64()
        .ok_or_else(invalid)?;
    if admission != run.admission_id {
        return Err(conflict("not_found", "no such admission for this run"));
    }
    if generation != run.execution_generation {
        return Err(conflict(
            "stale_generation",
            &format!(
                "the run is at execution_generation {}",
                run.execution_generation
            ),
        ));
    }
    let receipt = |status: &str| {
        json!({"run_id": run.id, "status": status, "admission_id": admission,
            "execution_generation": generation, "resolved": true})
    };
    match run.status.as_str() {
        "unknown" => {
            let update = RunUpdate {
                status: Some("interrupted".to_owned()),
                error: Some("the daemon running this run stopped; resolved as interrupted without replaying it".to_owned()),
                ..RunUpdate::default()
            };
            store
                .update_api_run(&run.id, &update)
                .map_err(|e| from_error(&e))?;
            shared.runs.resolved.notify_waiters();
            Ok(json_reply(200, &receipt("interrupted")))
        }
        "interrupted" => Ok(json_reply(200, &receipt("interrupted"))),
        _ => Err(conflict(
            "not_unknown",
            &format!("run {} is {}, not unknown", run.id, run.status),
        )),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::exact;

    #[test]
    fn control_bodies_are_exact() {
        let fields = ["request_id", "execution_generation", "choice"];
        assert!(
            exact(
                &json!({"request_id": "a", "execution_generation": 1, "choice": "once"}),
                &fields
            )
            .is_ok()
        );
        assert_eq!(
            exact(
                &json!({"request_id": "a", "execution_generation": 1}),
                &fields
            )
            .unwrap_err()
            .status(),
            400
        );
        assert_eq!(
            exact(
                &json!({"request_id": "a", "execution_generation": 1, "choice": "once", "x": 1}),
                &fields
            )
            .unwrap_err()
            .status(),
            400
        );
    }
}
