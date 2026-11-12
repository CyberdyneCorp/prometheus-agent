//! `POST /v1/responses` and the response store (spec: api-server,
//! Responses endpoint and response store). A response's `output` lists
//! only its own turn's items: executed `function_call` /
//! `function_call_output` items, `reasoning` summaries, commentary
//! messages and the final message. Chained requests
//! (`previous_response_id`, a named `conversation`, or the caller's
//! session key) continue one session; `reasoning` (and other output)
//! items a client echoes back as input are ignored.

use std::sync::Arc;

use pmt_store::api::StoredResponse;
use pmt_store::api::admissions::Endpoint;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::admission::Finish;
use crate::admission::{self, Gate};
use crate::auth::Caller;
use crate::content::{self, UserInput};
use crate::host::{ApiTurn, NewApiSession};
use crate::http::{Reply, Req, Sse, bad_request, from_error, json_reply, not_found, sse_reply};
use crate::responses_out::Builder;
use crate::session;
use crate::turn::{self, Answering};
use crate::{Shared, model};

/// The identity fields of a Responses request.
const IDENTITY: [&str; 10] = [
    "input",
    "instructions",
    "previous_response_id",
    "conversation",
    "conversation_history",
    "model",
    "provider",
    "model_options",
    "tools",
    "truncation",
];

/// What the request's `input` holds.
#[derive(Debug, Default)]
pub(crate) struct Input {
    /// Earlier user and assistant messages.
    pub earlier: Vec<(String, String)>,
    /// The new user message.
    pub last: UserInput,
}

/// Reads `input`: a string, or items of which only `message` items count
/// (output items echoed back, `reasoning` among them, are ignored).
pub(crate) fn input(value: &Value) -> Result<Input, Reply> {
    let items = match value {
        Value::String(text) => {
            return Ok(Input {
                earlier: Vec::new(),
                last: UserInput {
                    text: text.clone(),
                    images: Vec::new(),
                },
            });
        }
        Value::Array(items) => items,
        _ => {
            return Err(bad_request(
                "invalid_input",
                "input must be a string or a list of items",
            ));
        }
    };
    let messages: Vec<&Value> = items
        .iter()
        .filter(|i| i["type"].as_str().is_none_or(|t| t == "message") && i.get("role").is_some())
        .collect();
    let last_index = messages
        .iter()
        .rposition(|m| m["role"] == "user")
        .ok_or_else(|| bad_request("missing_user_message", "input must contain a user message"))?;
    let last = content::user_input(&messages[last_index]["content"])?;
    let earlier = messages[..last_index]
        .iter()
        .filter_map(|m| {
            let role = m["role"].as_str()?;
            matches!(role, "user" | "assistant")
                .then(|| (role.to_owned(), content::text_of(&m["content"])))
        })
        .collect();
    Ok(Input { earlier, last })
}

/// Answers `POST /v1/responses`.
pub(crate) async fn create(shared: &Arc<Shared>, caller: &Caller, req: &Req) -> Reply {
    match run(shared, caller, req).await {
        Ok(reply) | Err(reply) => reply,
    }
}

async fn run(shared: &Arc<Shared>, caller: &Caller, req: &Req) -> Result<Reply, Reply> {
    let body = req.json()?;
    let store = shared.store(&caller.profile)?;
    let config = shared.scope(&caller.profile)?.config().api_server.clone();
    let input = input(&body["input"])?;
    let key = session::session_key(req)?;
    let chained = chained_session(&store, &body, key.as_deref())?;
    let identity = admission::identity(&Value::Array(
        IDENTITY.iter().map(|f| body[*f].clone()).collect(),
    ));
    let idem = admission::key_of(req)?;
    let selected = model::select(shared, caller, &body, chained.as_deref(), true).await?;
    let request = admission::Request {
        endpoint: Endpoint::Responses,
        key: idem.as_deref(),
        identity: &identity,
        session: chained.as_deref(),
        conflict_code: "admission_conflict",
    };
    let stream = body["stream"].as_bool().unwrap_or(false);
    let finish =
        match admission::admit_run(shared, &store, caller, &request, config.max_concurrent_runs)
            .await?
        {
            Gate::Replay(answer) => return Ok(replay(&answer, stream)),
            Gate::Run(finish) => finish,
        };
    let instructions = body["instructions"].as_str().map(str::to_owned);
    let new = NewApiSession {
        instructions: instructions.clone(),
        session_key: key.clone(),
        derived_key: None,
        history: history(&body["conversation_history"]).unwrap_or(input.earlier.clone()),
        title: None,
    };
    let (session_id, additions) =
        match session::continue_or_create(shared, &caller.profile, chained, new).await {
            Ok(target) => target,
            Err(err) => {
                finish.release();
                return Err(from_error(&err));
            }
        };
    let route = selected.route();
    let turn = ApiTurn {
        session_id: session_id.clone(),
        text: input.last.text,
        images: input.last.images,
        additions,
        model: selected.choice,
        options: selected.options,
        unattended: Some(config.unattended_approvals),
    };
    let head = Head {
        id: format!("resp_{}", ulid::Ulid::generate()),
        model: config.model_id(&caller.profile),
        session: session_id,
        key,
        previous: body["previous_response_id"].as_str().map(str::to_owned),
        conversation: conversation_name(&body),
        instructions,
        store: body["store"].as_bool().unwrap_or(true),
        max_stored: config.responses.max_stored,
        tool_output_max: config.responses.history_tool_output_max_chars,
        route,
    };
    let (out, reply) = if stream {
        let (out, reply) = sse_reply();
        (Some(out), Some(reply))
    } else {
        (None, None)
    };
    let task = respond(
        Arc::clone(shared),
        caller.profile.clone(),
        turn,
        head,
        finish,
        out,
    );
    match reply {
        Some(reply) => {
            pmt_core::spawn::spawn(async move {
                let _ = task.await;
            });
            Ok(reply)
        }
        None => task.await,
    }
}

/// The session a request continues: its previous response's, its named
/// conversation's latest, or the newest of the caller's session key.
fn chained_session(
    store: &Arc<pmt_store::Store>,
    body: &Value,
    key: Option<&str>,
) -> Result<Option<String>, Reply> {
    if let Some(previous) = body["previous_response_id"].as_str() {
        let found = store.api_response(previous).map_err(|e| from_error(&e))?;
        return found
            .map(|r| Some(r.session_id))
            .ok_or_else(|| not_found(&format!("response {previous}")));
    }
    if let Some(name) = conversation_name(body) {
        let latest = store
            .api_conversation_latest(&name)
            .map_err(|e| from_error(&e))?;
        return Ok(latest.map(|r| r.session_id));
    }
    if history(&body["conversation_history"]).is_some() {
        return Ok(None);
    }
    match key {
        Some(key) => Ok(store
            .api_session_by_key(key)
            .map_err(|e| from_error(&e))?
            .map(|id| id.to_string())),
        None => Ok(None),
    }
}

fn conversation_name(body: &Value) -> Option<String> {
    body["conversation"]
        .as_str()
        .or_else(|| body["conversation"]["id"].as_str())
        .map(str::to_owned)
}

/// A non-empty `conversation_history` (authoritative history).
pub(crate) fn history(value: &Value) -> Option<Vec<(String, String)>> {
    let items = value.as_array().filter(|items| !items.is_empty())?;
    Some(
        items
            .iter()
            .filter_map(|m| {
                let role = m["role"].as_str()?;
                matches!(role, "user" | "assistant")
                    .then(|| (role.to_owned(), content::text_of(&m["content"])))
            })
            .collect(),
    )
}

/// What a response object carries besides its turn.
pub(crate) struct Head {
    pub id: String,
    pub model: String,
    pub session: String,
    pub key: Option<String>,
    pub previous: Option<String>,
    pub conversation: Option<String>,
    pub instructions: Option<String>,
    pub store: bool,
    pub max_stored: u32,
    pub tool_output_max: u64,
    /// [`crate::model::Selected::route`] of the request.
    pub route: Value,
}

async fn respond(
    shared: Arc<Shared>,
    profile: String,
    turn: ApiTurn,
    head: Head,
    finish: Finish,
    out: Option<mpsc::UnboundedSender<Sse>>,
) -> Result<Reply, Reply> {
    let mut builder = Builder::new(&head, out);
    builder.started();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let run = turn::run(&shared, &profile, turn, &tx, Answering::Nobody);
    tokio::pin!(run);
    let outcome = loop {
        tokio::select! {
            outcome = &mut run => break outcome,
            Some(event) = rx.recv() => builder.event(event),
        }
    };
    while let Ok(event) = rx.try_recv() {
        builder.event(event);
    }
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(err) => {
            finish.release();
            builder.refused(&err);
            return Err(from_error(&err));
        }
    };
    let response = builder.finished(&outcome);
    finish.settle(&head.session, &response);
    if head.store {
        let stored = StoredResponse {
            id: head.id.clone(),
            session_id: head.session.clone(),
            conversation: head.conversation.clone(),
            body: crate::responses_out::for_storage(&response, head.tool_output_max).to_string(),
            created_at: jiff::Timestamp::now(),
        };
        if let Err(err) = finish.store.put_api_response(&stored, head.max_stored) {
            tracing::warn!("response {} not stored: {err}", head.id);
        }
    }
    Ok(session::echo(
        json_reply(200, &response),
        &head.session,
        head.key.as_deref(),
    ))
}

/// The stored answer of an exact retry.
fn replay(answer: &str, stream: bool) -> Reply {
    let response: Value = serde_json::from_str(answer).unwrap_or(Value::Null);
    let session = response["prometheus"]["session_id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let key = response["prometheus"]["session_key"]
        .as_str()
        .map(str::to_owned);
    let reply = if stream {
        let (out, reply) = sse_reply();
        crate::responses_out::replay_stream(&response, &out);
        reply
    } else {
        json_reply(200, &response)
    };
    let reply = crate::http::with_header(reply, "idempotency-replayed", "true");
    session::echo(reply, &session, key.as_deref())
}

/// `GET /v1/responses/{id}`.
pub(crate) fn get(shared: &Shared, caller: &Caller, id: &str) -> Reply {
    let store = match shared.store(&caller.profile) {
        Ok(store) => store,
        Err(reply) => return reply,
    };
    match store.api_response(id) {
        Ok(Some(stored)) => {
            let body: Value = serde_json::from_str(&stored.body).unwrap_or(Value::Null);
            json_reply(200, &body)
        }
        Ok(None) => not_found(&format!("response {id}")),
        Err(err) => from_error(&err),
    }
}

/// `DELETE /v1/responses/{id}`.
pub(crate) fn delete(shared: &Shared, caller: &Caller, id: &str) -> Reply {
    let store = match shared.store(&caller.profile) {
        Ok(store) => store,
        Err(reply) => return reply,
    };
    match store.delete_api_response(id) {
        Ok(true) => json_reply(
            200,
            &json!({"id": id, "object": "response.deleted", "deleted": true}),
        ),
        Ok(false) => not_found(&format!("response {id}")),
        Err(err) => from_error(&err),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::input;

    /// Scenario: Echoed reasoning items ignored on input.
    #[test]
    fn echoed_output_items_are_not_user_turns() {
        let parsed = input(&json!([
            {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "thinking"}]},
            {"type": "function_call", "call_id": "c1", "name": "terminal", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c1", "output": "a.txt"},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "done"}]},
            {"role": "user", "content": "Now show me the README"},
        ]))
        .unwrap();
        assert_eq!(parsed.last.text, "Now show me the README");
        assert_eq!(
            parsed.earlier,
            vec![("assistant".to_owned(), "done".to_owned())]
        );
        assert!(input(&json!([{"type": "reasoning", "summary": []}])).is_err());
    }
}
