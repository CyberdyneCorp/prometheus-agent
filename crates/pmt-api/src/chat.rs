//! `POST /v1/chat/completions` (spec: api-server, Chat Completions
//! endpoint; API session identity and ownership; Idempotent admission).
//! One agent turn for the last user message, answered as a
//! `chat.completion` or as `chat.completion.chunk` events ending in
//! `data: [DONE]`. Clients send their whole history every time: with
//! `X-Prometheus-Session-Id` the session's stored history is used and the
//! earlier client messages are ignored; without it the conversation is
//! the API session derived from the system prompt and the first user
//! message.

use pmt_store::api::admissions::Endpoint;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::admission::{self, Finish, Gate};
use crate::auth::Caller;
use crate::content::{self, UserInput};
use crate::host::{ApiTurn, NewApiSession};
use crate::http::{Reply, Req, Sse, bad_request, from_error, json_reply, sse_reply};
use crate::session::{self, SESSION_ID};
use crate::turn::{self, Answering, Outcome, TurnEvent};
use crate::{Shared, model};

/// A request's messages, split.
struct Conversation {
    system: Option<String>,
    /// Earlier user and assistant turns (text only).
    earlier: Vec<(String, String)>,
    /// The first user message's text (the derived session key).
    first_user: String,
    last: UserInput,
    /// The last user message as sent (its identity).
    last_raw: Value,
}

impl Conversation {
    fn parse(messages: &[Value]) -> Result<Self, Reply> {
        let last_index = messages
            .iter()
            .rposition(|m| m["role"] == "user")
            .ok_or_else(|| {
                bad_request(
                    "missing_user_message",
                    "messages must contain a user message",
                )
            })?;
        let last = content::user_input(&messages[last_index]["content"])?;
        let mut system = Vec::new();
        let mut earlier = Vec::new();
        for message in &messages[..last_index] {
            let role = message["role"].as_str().unwrap_or_default();
            let text = content::text_of(&message["content"]);
            match role {
                "system" | "developer" => system.push(text),
                "user" => {
                    content::user_input(&message["content"])?;
                    earlier.push(("user".to_owned(), text));
                }
                "assistant" if !text.is_empty() => earlier.push(("assistant".to_owned(), text)),
                _ => {}
            }
        }
        let first_user = earlier
            .iter()
            .find(|(role, _)| role == "user")
            .map_or_else(|| last.text.clone(), |(_, text)| text.clone());
        Ok(Self {
            system: (!system.is_empty()).then(|| system.join("\n\n")),
            earlier,
            first_user,
            last,
            last_raw: messages[last_index]["content"].clone(),
        })
    }

    /// The stable key of a conversation without a session header.
    fn derived_key(&self) -> String {
        admission::identity(&json!([self.system, self.first_user]))
    }
}

/// Answers the request.
pub(crate) async fn complete(shared: &std::sync::Arc<Shared>, caller: &Caller, req: &Req) -> Reply {
    match run(shared, caller, req).await {
        Ok(reply) | Err(reply) => reply,
    }
}

async fn run(shared: &std::sync::Arc<Shared>, caller: &Caller, req: &Req) -> Result<Reply, Reply> {
    let body = req.json()?;
    let store = shared.store(&caller.profile)?;
    let scope = shared.scope(&caller.profile)?;
    let messages = body["messages"]
        .as_array()
        .ok_or_else(|| bad_request("missing_messages", "messages must be a list"))?;
    let conversation = Conversation::parse(messages)?;
    let key = session::session_key(req)?;
    let header = req.header_any(&SESSION_ID).map(str::to_owned);
    if let Some(id) = &header {
        session::owned(&store, id)?;
    }
    let stream = body["stream"].as_bool().unwrap_or(false);
    let identity = admission::identity(&json!({
        "last": conversation.last_raw,
        "earlier": if header.is_some() { Value::Null } else { json!(conversation.earlier) },
        "system": conversation.system,
        "session": header,
        "model": body["model"],
        "provider": body["provider"],
        "model_options": body["model_options"],
    }));
    let idem = admission::key_of(req)?;
    let selected = model::select(shared, caller, &body, header.as_deref(), true).await?;
    let request = admission::Request {
        endpoint: Endpoint::Chat,
        key: idem.as_deref(),
        identity: &identity,
        session: header.as_deref(),
        conflict_code: "admission_conflict",
    };
    let config = scope.config().api_server.clone();
    let finish =
        match admission::admit_run(shared, &store, caller, &request, config.max_concurrent_runs)
            .await?
        {
            Gate::Replay(answer) => return Ok(replay(&answer, stream)),
            Gate::Run(finish) => finish,
        };
    let (session_id, additions) = target(shared, caller, &conversation, header, key.clone())
        .await
        .inspect_err(|_| finish.release())?;
    let route = selected.route();
    let turn = ApiTurn {
        session_id: session_id.clone(),
        text: conversation.last.text,
        images: conversation.last.images,
        additions,
        model: selected.choice,
        options: selected.options,
        unattended: Some(config.unattended_approvals),
    };
    let shape = Shape {
        model: config.model_id(&caller.profile),
        session: session_id,
        key,
        progress: config.tool_progress_events,
        route,
    };
    if stream {
        Ok(streamed(
            std::sync::Arc::clone(shared),
            caller.profile.clone(),
            turn,
            shape,
            finish,
        ))
    } else {
        answer_now(shared, &caller.profile, turn, &shape, finish).await
    }
}

/// Runs the turn and answers it as one `chat.completion`.
async fn answer_now(
    shared: &Shared,
    profile: &str,
    turn: ApiTurn,
    shape: &Shape,
    finish: Finish,
) -> Result<Reply, Reply> {
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = turn::run(shared, profile, turn, &tx, Answering::Nobody)
        .await
        .map_err(|err| {
            finish.release();
            from_error(&err)
        })?;
    let completion = completion(shape, &outcome);
    finish.settle(&shape.session, &completion);
    Ok(session::echo(
        json_reply(200, &completion),
        &shape.session,
        shape.key.as_deref(),
    ))
}

/// The session the turn runs in, and the notices of changed caller
/// instructions.
async fn target(
    shared: &Shared,
    caller: &Caller,
    conversation: &Conversation,
    header: Option<String>,
    key: Option<String>,
) -> Result<(String, Vec<String>), Reply> {
    let store = shared.store(&caller.profile)?;
    if let Some(id) = header {
        let fixed = id
            .parse()
            .ok()
            .and_then(|sid| store.api_session(sid).ok().flatten())
            .and_then(|s| s.instructions);
        let additions = changed_instructions(fixed.as_deref(), conversation.system.as_deref());
        return Ok((id, additions));
    }
    let derived = conversation.derived_key();
    if let Some(found) = store
        .api_session_by_derived(&derived)
        .map_err(|e| from_error(&e))?
    {
        return Ok((found.to_string(), Vec::new()));
    }
    let new = NewApiSession {
        instructions: conversation.system.clone(),
        session_key: key,
        derived_key: Some(derived),
        history: conversation.earlier.clone(),
        title: None,
    };
    let id = shared
        .host
        .api_create_session(&caller.profile, new)
        .await
        .map_err(|e| from_error(&e))?;
    Ok((id, Vec::new()))
}

/// A notice for this turn when the caller's instructions differ from the
/// ones the session was created with (its system prompt never changes).
pub(crate) fn changed_instructions(fixed: Option<&str>, sent: Option<&str>) -> Vec<String> {
    match sent {
        Some(sent) if Some(sent) != fixed => vec![format!(
            "[System notice from the API caller: for this turn the caller's instructions are]\n{sent}"
        )],
        _ => Vec::new(),
    }
}

/// How the answer is shaped.
pub(crate) struct Shape {
    pub model: String,
    pub session: String,
    pub key: Option<String>,
    pub progress: bool,
    /// [`model::Selected::route`] of the request.
    pub route: Value,
}

/// `finish_reason` and the `prometheus.error_code` of an outcome.
fn finish_reason(outcome: &Outcome) -> (&'static str, Option<String>) {
    match outcome.status() {
        "completed" => ("stop", None),
        "incomplete" => ("length", Some("iteration_budget_exhausted".to_owned())),
        "cancelled" => ("cancelled", None),
        _ => (
            "error",
            Some(
                outcome
                    .error
                    .as_ref()
                    .map_or_else(|| outcome.exit_reason.clone(), |e| e.code.clone()),
            ),
        ),
    }
}

fn prometheus_block(shape: &Shape, outcome: &Outcome, error_code: Option<String>) -> Value {
    let mut block = session::stamp(&shape.session, shape.key.as_deref());
    block["exit_reason"] = outcome.exit_reason.clone().into();
    if let Some(code) = error_code {
        block["error_code"] = code.into();
        if outcome.status() == "failed" {
            block["error"] = outcome.error_text().into();
        }
    }
    if let Some(runtime) = &outcome.runtime {
        block["runtime"] = model::runtime(runtime, &shape.route);
    }
    block
}

fn reasoning_of(outcome: &Outcome) -> Option<String> {
    let text: Vec<&str> = outcome
        .items
        .iter()
        .filter_map(|i| match i {
            turn::Item::Reasoning(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    (!text.is_empty()).then(|| text.join("\n"))
}

/// The `chat.completion` of a finished turn.
fn completion(shape: &Shape, outcome: &Outcome) -> Value {
    let (reason, code) = finish_reason(outcome);
    let mut message = json!({"role": "assistant", "content": outcome.final_response});
    if let Some(reasoning) = reasoning_of(outcome) {
        message["reasoning_content"] = reasoning.into();
    }
    json!({
        "id": format!("chatcmpl-{}", ulid::Ulid::generate()),
        "object": "chat.completion",
        "created": jiff::Timestamp::now().as_second(),
        "model": shape.model,
        "choices": [{"index": 0, "message": message, "finish_reason": reason, "logprobs": null}],
        "usage": outcome.openai_usage(),
        "prometheus": prometheus_block(shape, outcome, code),
    })
}

fn chunk(id: &str, created: i64, model: &str, delta: &Value, finish: Option<&str>) -> Value {
    json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish, "logprobs": null}],
    })
}

/// Streams the turn as `chat.completion.chunk` events.
fn streamed(
    shared: std::sync::Arc<Shared>,
    profile: String,
    turn: ApiTurn,
    shape: Shape,
    finish: Finish,
) -> Reply {
    let (out, reply) = sse_reply();
    let reply = session::echo(reply, &shape.session, shape.key.as_deref());
    pmt_core::spawn::spawn(async move {
        let id = format!("chatcmpl-{}", ulid::Ulid::generate());
        let created = jiff::Timestamp::now().as_second();
        let stamp = session::stamp(&shape.session, shape.key.as_deref());
        let send = |delta: Value, finish_reason: Option<&str>| {
            let mut frame = chunk(&id, created, &shape.model, &delta, finish_reason);
            frame["prometheus"] = stamp.clone();
            let _ = out.send(Sse::data(&frame));
        };
        send(json!({"role": "assistant", "content": ""}), None);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let run = turn::run(&shared, &profile, turn, &tx, Answering::Nobody);
        tokio::pin!(run);
        let mut streamed = false;
        let outcome = loop {
            tokio::select! {
                outcome = &mut run => break outcome,
                Some(event) = rx.recv() => {
                    streamed |= forward(&out, &send, &shape, event);
                }
            }
        };
        while let Ok(event) = rx.try_recv() {
            streamed |= forward(&out, &send, &shape, event);
        }
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(err) => {
                finish.release();
                send(json!({}), Some("error"));
                let _ = out.send(Sse::data(
                    &json!({"error": {"message": err.message(), "code": err.code().code}}),
                ));
                let _ = out.send(Sse {
                    id: None,
                    event: None,
                    data: "[DONE]".into(),
                });
                return;
            }
        };
        if !streamed && let Some(text) = &outcome.final_response {
            send(json!({"content": text}), None);
        }
        let completion = completion(&shape, &outcome);
        finish.settle(&shape.session, &completion);
        let (reason, _) = finish_reason(&outcome);
        let mut last = chunk(&id, created, &shape.model, &json!({}), Some(reason));
        last["usage"] = completion["usage"].clone();
        last["prometheus"] = completion["prometheus"].clone();
        let _ = out.send(Sse::data(&last));
        let _ = out.send(Sse {
            id: None,
            event: None,
            data: "[DONE]".into(),
        });
    });
    reply
}

/// Sends one turn event; whether it streamed answer text.
fn forward(
    out: &mpsc::UnboundedSender<Sse>,
    send: &impl Fn(Value, Option<&str>),
    shape: &Shape,
    event: TurnEvent,
) -> bool {
    match event {
        TurnEvent::Delta(text) => {
            send(json!({"content": text}), None);
            return true;
        }
        TurnEvent::Reasoning(text) => send(json!({"reasoning_content": text}), None),
        TurnEvent::ToolStarted {
            call_id,
            tool,
            arguments,
        } if shape.progress => {
            let _ = out.send(Sse::named(
                "prometheus.tool.progress",
                &json!({"tool": tool, "call_id": call_id, "status": "started",
                    "preview": crate::runs::preview(&arguments.to_string()),
                    "session_id": shape.session}),
            ));
        }
        _ => {}
    }
    false
}

/// The stored answer of an exact retry, as JSON or as a stream.
fn replay(answer: &str, stream: bool) -> Reply {
    let completion: Value = serde_json::from_str(answer).unwrap_or(Value::Null);
    let session = completion["prometheus"]["session_id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let key = completion["prometheus"]["session_key"]
        .as_str()
        .map(str::to_owned);
    let reply = if stream {
        let (out, reply) = sse_reply();
        let id = completion["id"].as_str().unwrap_or_default();
        let created = completion["created"].as_i64().unwrap_or_default();
        let model = completion["model"].as_str().unwrap_or_default();
        let choice = &completion["choices"][0];
        let mut frames = vec![chunk(
            id,
            created,
            model,
            &json!({"role": "assistant", "content": ""}),
            None,
        )];
        if let Some(reasoning) = choice["message"]["reasoning_content"].as_str() {
            frames.push(chunk(
                id,
                created,
                model,
                &json!({"reasoning_content": reasoning}),
                None,
            ));
        }
        if let Some(text) = choice["message"]["content"].as_str() {
            frames.push(chunk(id, created, model, &json!({"content": text}), None));
        }
        let mut last = chunk(
            id,
            created,
            model,
            &json!({}),
            choice["finish_reason"].as_str(),
        );
        last["usage"] = completion["usage"].clone();
        frames.push(last);
        for mut frame in frames {
            frame["prometheus"] = completion["prometheus"].clone();
            let _ = out.send(Sse::data(&frame));
        }
        let _ = out.send(Sse {
            id: None,
            event: None,
            data: "[DONE]".into(),
        });
        reply
    } else {
        json_reply(200, &completion)
    };
    let reply = crate::http::with_header(reply, "idempotency-replayed", "true");
    session::echo(reply, &session, key.as_deref())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Conversation, Outcome, Shape, completion};

    fn shape() -> Shape {
        Shape {
            model: "prometheus-agent".into(),
            session: "s1".into(),
            key: None,
            progress: true,
            route: json!({"route_source": "global", "requested": null}),
        }
    }

    /// Scenario: Budget exhausted is not success.
    #[test]
    fn an_exhausted_iteration_budget_finishes_with_length() {
        let outcome = Outcome {
            exit_reason: "iteration_limit".into(),
            final_response: Some("wrap-up".into()),
            usage: json!({"input_tokens": 10, "output_tokens": 5, "cache_read_tokens": 3, "cache_write_tokens": 2}),
            ..Outcome::default()
        };
        let answer = completion(&shape(), &outcome);
        assert_eq!(answer["choices"][0]["finish_reason"], "length");
        assert_eq!(
            answer["prometheus"]["error_code"],
            "iteration_budget_exhausted"
        );
        assert_eq!(answer["usage"]["prompt_tokens"], 15);
        assert_eq!(answer["usage"]["cache_read_tokens"], 3);
        assert_eq!(answer["usage"]["cache_write_tokens"], 2);
        for (reason, expected) in [
            ("completed", "stop"),
            ("interrupted_by_user", "cancelled"),
            ("provider_failed", "error"),
        ] {
            let outcome = Outcome {
                exit_reason: reason.into(),
                ..Outcome::default()
            };
            assert_eq!(
                completion(&shape(), &outcome)["choices"][0]["finish_reason"],
                expected
            );
        }
    }

    #[test]
    fn the_last_user_message_is_the_turn_and_the_rest_is_history() {
        let conversation = Conversation::parse(&[
            json!({"role": "system", "content": "Always answer in French"}),
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "salut"}),
            json!({"role": "user", "content": [{"type": "text", "text": "list files"}]}),
        ])
        .unwrap();
        assert_eq!(
            conversation.system.as_deref(),
            Some("Always answer in French")
        );
        assert_eq!(conversation.earlier.len(), 2);
        assert_eq!(conversation.first_user, "hi");
        assert_eq!(conversation.last.text, "list files");
    }
}
