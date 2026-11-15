//! `/api/kanban/...` (spec: web-dashboard, Kanban dashboard API and
//! orchestration modes): the shared boards, the orchestration settings,
//! active workers, one run, Terminate and the dispatcher's inspect view,
//! answered by the daemon ([`crate::host::KanbanCall`]). The API router
//! has already checked the session and CSRF; here observers are refused
//! every change, and so is a device bound to profiles that do not include
//! the dispatcher's (whose `config.toml` the settings live in, and whose
//! dispatcher runs every worker). Attachment uploads are refused above the
//! dispatcher profile's `kanban.attachment_max_bytes` by their declared
//! length, before any byte is buffered, and again while they stream.

use hyper::Method;
use hyper::body::Incoming;
use hyper::header::{CONTENT_LENGTH, CONTENT_TYPE};

use super::Caller;
use crate::Shared;
use crate::auth::sessions::Role;
use crate::host::{KanbanCall, KanbanUpload};
use crate::http::{Reply, Req, error, from_error, json_reply, not_found, read_body, read_json};

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
        (&Method::POST, ["boards", board, "tasks", task, "attachments"]) => {
            match upload(shared, caller, req, (board, task), body).await {
                Ok(upload) => KanbanCall::Attach(upload),
                Err(reply) => return reply,
            }
        }
        _ => return not_found(),
    };
    match shared.host.kanban(call).await {
        Ok(value) => json_reply(200, &value),
        Err(err) => from_error(&err),
    }
}

fn too_large(size: impl std::fmt::Display, limit: u64) -> Reply {
    error(
        413,
        "kanban.attachment_too_large",
        &format!("attachment too large: {size} > {limit}"),
    )
}

/// Reads an upload's body under the dispatcher profile's attachment limit:
/// a declared length above it is refused unread; a body that streams past
/// it is cut off. Nothing reaches the blob store either way.
async fn upload(
    shared: &Shared,
    caller: &Caller,
    req: &Req,
    (board, task): (&str, &str),
    body: Incoming,
) -> Result<KanbanUpload, Reply> {
    let limit = shared
        .host
        .kanban(KanbanCall::AttachmentLimit)
        .await
        .map_err(|e| from_error(&e))?["max_bytes"]
        .as_u64()
        .unwrap_or(0);
    let Some(declared) = req
        .header(CONTENT_LENGTH.as_str())
        .and_then(|v| v.parse::<u64>().ok())
    else {
        return Err(error(
            411,
            "length_required",
            "an attachment upload needs a Content-Length",
        ));
    };
    if declared > limit {
        return Err(too_large(declared, limit));
    }
    let Some(bytes) = read_body(body, usize::try_from(limit).unwrap_or(usize::MAX)).await else {
        return Err(too_large(format!("more than {limit}"), limit));
    };
    Ok(KanbanUpload {
        board: board.to_owned(),
        task: task.to_owned(),
        name: req.param("name").unwrap_or_default(),
        content_type: req.header(CONTENT_TYPE.as_str()).map(str::to_owned),
        bytes: bytes.to_vec(),
        who: caller.signed.subject.clone(),
    })
}
