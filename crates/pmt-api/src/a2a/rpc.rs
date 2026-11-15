//! The A2A JSON-RPC endpoint (spec: agent-interop, A2A JSON-RPC methods
//! and task lifecycle; A2A authentication, trust and rate limits; A2A push
//! notifications): the caller is identified, rate limited, and its method
//! dispatched in the dialect it was called in. A caller sees only its own
//! tasks.

use std::net::IpAddr;
use std::sync::Arc;

use pmt_a2a::wire::{self, Dialect, Method, TaskState, code};
use pmt_config::schema::A2aServerConfig;
use serde_json::{Value, json};

use super::tasks::PushConfig;
use super::{A2a, gate, push, refused, run};
use crate::http::{Reply, Req, json_reply};

/// The refusal of a push configuration nothing can sign.
pub(super) const NO_PUSH_SECRET: &str =
    "push notifications need a2a.server.push_secret or a peer token to sign them";

/// One call's context.
pub(super) struct Ctx<'a> {
    pub a2a: &'a Arc<A2a>,
    pub profile: &'a str,
    pub config: &'a A2aServerConfig,
    /// The authenticated caller.
    pub identity: &'a str,
    /// The token it authenticated with.
    pub token: Option<String>,
    pub dialect: Dialect,
    /// The JSON-RPC request id.
    pub id: &'a Value,
}

/// A JSON-RPC result.
pub(super) fn answer(ctx: &Ctx<'_>, result: Value) -> Reply {
    json_reply(200, &wire::ok(ctx.id, result))
}

/// A JSON-RPC error.
pub(super) fn failure(ctx: &Ctx<'_>, code: i64, message: &str) -> Reply {
    json_reply(200, &wire::error(ctx.id, code, message))
}

/// Handles one JSON-RPC request.
pub(super) async fn call(
    a2a: &Arc<A2a>,
    profile: &str,
    config: &A2aServerConfig,
    req: &Req,
    caller: IpAddr,
) -> Reply {
    let Ok(body) = serde_json::from_slice::<Value>(&req.body) else {
        return json_reply(
            200,
            &wire::error(&Value::Null, code::PARSE, "the body is not JSON"),
        );
    };
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    let (identity, token) = if config.has_tokens() {
        let secret = |name: &str| a2a.api.host.api_secret(profile, name);
        match gate::identify(req, config, &secret, caller) {
            Ok(found) => found,
            Err(refusal) => return refused(&refusal),
        }
    } else {
        (gate::LOCAL.to_owned(), None)
    };
    if !a2a
        .limits
        .admit(profile, &identity, config.rate_limit_per_minute)
    {
        let message = format!(
            "rate limit exceeded ({} requests per minute)",
            config.rate_limit_per_minute
        );
        return json_reply(429, &wire::error(&id, code::RATE_LIMITED, &message));
    }
    let Some(name) = body["method"].as_str() else {
        return json_reply(
            200,
            &wire::error(&id, code::INVALID_REQUEST, "not a JSON-RPC request"),
        );
    };
    let Some((method, dialect)) = wire::method(name) else {
        return json_reply(
            200,
            &wire::error(
                &id,
                code::METHOD_NOT_FOUND,
                &format!("method not found: {name}"),
            ),
        );
    };
    let ctx = Ctx {
        a2a,
        profile,
        config,
        identity: &identity,
        token,
        dialect,
        id: &id,
    };
    dispatch(&ctx, method, &body["params"]).await
}

async fn dispatch(ctx: &Ctx<'_>, method: Method, params: &Value) -> Reply {
    match method {
        Method::SendMessage => run::send(ctx, params, false).await,
        Method::SendStreamingMessage => run::send(ctx, params, true).await,
        Method::GetTask => get(ctx, params),
        Method::ListTasks => list(ctx, params),
        Method::CancelTask => cancel(ctx, params).await,
        Method::SubscribeToTask => match task_of(ctx, params, "id") {
            Ok(task) => run::stream_task(ctx, &task.id),
            Err(reply) => reply,
        },
        Method::CreatePush => push_create(ctx, params).await,
        Method::GetPush | Method::ListPush | Method::DeletePush => push_read(ctx, method, params),
    }
}

/// The caller's task named by `params[key]`.
fn task_of(ctx: &Ctx<'_>, params: &Value, key: &str) -> Result<super::tasks::Task, Reply> {
    let Some(id) = params[key].as_str() else {
        return Err(failure(
            ctx,
            code::INVALID_PARAMS,
            &format!("params.{key} is required"),
        ));
    };
    ctx.a2a
        .tasks
        .get(ctx.profile, ctx.identity, id)
        .ok_or_else(|| failure(ctx, code::TASK_NOT_FOUND, &format!("no task {id}")))
}

fn history_length(params: &Value) -> Option<usize> {
    params["historyLength"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
}

fn get(ctx: &Ctx<'_>, params: &Value) -> Reply {
    match task_of(ctx, params, "id") {
        Ok(task) => answer(ctx, task.json(ctx.dialect, history_length(params))),
        Err(reply) => reply,
    }
}

fn list(ctx: &Ctx<'_>, params: &Value) -> Reply {
    let state = params["status"].as_str().and_then(TaskState::parse);
    let tasks = ctx.a2a.tasks.list(
        ctx.profile,
        ctx.identity,
        params["contextId"].as_str(),
        state,
    );
    let size = params["pageSize"]
        .as_u64()
        .map_or(50, |n| usize::try_from(n).unwrap_or(100))
        .clamp(1, 100);
    let start: usize = params["pageToken"]
        .as_str()
        .and_then(|t| t.parse().ok())
        .unwrap_or(0);
    let page: Vec<Value> = tasks
        .iter()
        .skip(start)
        .take(size)
        .map(|t| t.json(ctx.dialect, history_length(params)))
        .collect();
    let next = if start + size < tasks.len() {
        (start + size).to_string()
    } else {
        String::new()
    };
    answer(
        ctx,
        json!({"tasks": page, "nextPageToken": next, "pageSize": size, "totalSize": tasks.len()}),
    )
}

async fn cancel(ctx: &Ctx<'_>, params: &Value) -> Reply {
    let task = match task_of(ctx, params, "id") {
        Ok(task) => task,
        Err(reply) => return reply,
    };
    let Some(cancelled) = ctx
        .a2a
        .tasks
        .transition(&task.id, TaskState::Canceled, None, None)
    else {
        return failure(
            ctx,
            code::TASK_NOT_CANCELABLE,
            &format!("task {} already ended", task.id),
        );
    };
    if let Some(session) = &cancelled.session
        && let Err(err) = ctx.a2a.api.host.interrupt(ctx.profile, session).await
    {
        tracing::debug!(task = task.id, "a2a cancel: {err}");
    }
    push::notify(ctx.a2a, &cancelled);
    answer(ctx, cancelled.json(ctx.dialect, None))
}

/// The secret signing pushes of this caller.
pub(super) fn push_secret(ctx: &Ctx<'_>) -> Option<String> {
    match &ctx.config.push_secret {
        Some(name) => ctx
            .a2a
            .api
            .host
            .api_secret(ctx.profile, &name.0)
            .map(|s| s.expose().to_owned()),
        None => ctx.token.clone(),
    }
}

/// A push configuration whose URL the egress policy admits.
pub(super) async fn checked_push(ctx: &Ctx<'_>, config: &Value) -> Result<PushConfig, Reply> {
    let Some((_, config)) = push::parse_config(config) else {
        return Err(failure(
            ctx,
            code::INVALID_PARAMS,
            "the push configuration has no url",
        ));
    };
    if let Err(err) = ctx
        .a2a
        .api
        .host
        .api_push_check(ctx.profile, &config.url)
        .await
    {
        let message = format!("push URL refused by the egress policy: {}", err.message());
        return Err(failure(ctx, code::INVALID_PARAMS, &message));
    }
    Ok(config)
}

async fn push_create(ctx: &Ctx<'_>, params: &Value) -> Reply {
    let task = match task_of(ctx, params, "taskId") {
        Ok(task) => task,
        Err(reply) => return reply,
    };
    let config = match checked_push(ctx, params).await {
        Ok(config) => config,
        Err(reply) => return reply,
    };
    let Some(secret) = push_secret(ctx) else {
        return failure(ctx, code::PUSH_NOT_SUPPORTED, NO_PUSH_SECRET);
    };
    let stored = config.clone();
    ctx.a2a.tasks.update(&task.id, move |t| {
        t.push.retain(|c| c.id != stored.id);
        t.push.push(stored);
        t.push_secret = Some(secret);
    });
    answer(ctx, push::config_json(&task.id, &config, ctx.dialect))
}

fn push_read(ctx: &Ctx<'_>, method: Method, params: &Value) -> Reply {
    let task_key = if params["taskId"].is_string() {
        "taskId"
    } else {
        "id"
    };
    let task = match task_of(ctx, params, task_key) {
        Ok(task) => task,
        Err(reply) => return reply,
    };
    let config_id = params["pushNotificationConfigId"]
        .as_str()
        .or_else(|| params["id"].as_str().filter(|_| task_key == "taskId"));
    match method {
        Method::ListPush => {
            let configs: Vec<Value> = task
                .push
                .iter()
                .map(|c| push::config_json(&task.id, c, ctx.dialect))
                .collect();
            match ctx.dialect {
                Dialect::V1 => answer(ctx, json!({"configs": configs, "nextPageToken": ""})),
                Dialect::V03 => answer(ctx, Value::Array(configs)),
            }
        }
        Method::DeletePush => {
            let Some(config_id) = config_id.map(str::to_owned) else {
                return failure(
                    ctx,
                    code::INVALID_PARAMS,
                    "the push configuration id is required",
                );
            };
            ctx.a2a
                .tasks
                .update(&task.id, |t| t.push.retain(|c| c.id != config_id));
            answer(ctx, json!({}))
        }
        _ => {
            let found = task
                .push
                .iter()
                .find(|c| config_id.is_none_or(|id| c.id == id));
            match found {
                Some(config) => answer(ctx, push::config_json(&task.id, config, ctx.dialect)),
                None => failure(ctx, code::INVALID_PARAMS, "no such push configuration"),
            }
        }
    }
}
