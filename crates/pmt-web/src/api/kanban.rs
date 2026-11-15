//! `/api/kanban/...` (spec: web-dashboard, Kanban dashboard API and
//! orchestration modes): the shared boards, the orchestration settings,
//! active workers, one run, Terminate and the dispatcher's inspect view,
//! answered by the daemon ([`crate::host::KanbanCall`]). The API router
//! has already checked the session and CSRF; here observers are refused
//! every change, and so is a device bound to profiles that do not include
//! the dispatcher's (whose `config.toml` the settings live in, and whose
//! dispatcher runs every worker).

use hyper::Method;
use hyper::body::Incoming;

use super::Caller;
use crate::Shared;
use crate::auth::sessions::Role;
use crate::host::KanbanCall;
use crate::http::{Reply, Req, error, from_error, json_reply, not_found, read_json};

pub(crate) async fn route(
    shared: &Shared,
    caller: &Caller,
    req: &Req,
    rest: &[&str],
    body: Incoming,
) -> Reply {
    let reads = matches!(req.method, Method::GET | Method::HEAD);
    if !reads && caller.signed.role == Role::Observer {
        return error(403, "observer", "observers have read-only access");
    }
    if !reads && !caller.signed.may_use(&shared.host.launch_profile()) {
        return error(
            403,
            "profile",
            "kanban changes need access to the dispatcher's profile",
        );
    }
    let call = match (&req.method, rest) {
        (&Method::GET, ["boards"]) => KanbanCall::Boards,
        (&Method::GET, ["orchestration"]) => KanbanCall::Orchestration,
        (&Method::PUT, ["orchestration"]) => match read_json(body).await {
            Ok(body) => KanbanCall::SetOrchestration(body),
            Err(reply) => return reply,
        },
        (&Method::GET, ["workers", "active"]) => KanbanCall::Workers,
        (&Method::GET, ["runs", id]) => KanbanCall::Run((*id).to_owned()),
        (&Method::POST, ["runs", id, "terminate"]) => KanbanCall::Terminate {
            run: (*id).to_owned(),
            who: caller.signed.subject.clone(),
        },
        (&Method::GET, ["inspect"]) => KanbanCall::Inspect,
        _ => return not_found(),
    };
    match shared.host.kanban(call).await {
        Ok(value) => json_reply(200, &value),
        Err(err) => from_error(&err),
    }
}
