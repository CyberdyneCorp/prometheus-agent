//! The outbound A2A client (spec: agent-interop, Outbound A2A tools): fetch
//! an agent's card, send it a message and wait for the task to end. HTTP
//! goes through a [`Transport`] the caller supplies (the binary's is
//! egress-checked), so this module holds only the protocol.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde_json::{Value, json};

use crate::card::CardSummary;
use crate::wire::{self, Answer, Dialect, TaskState};

/// A boxed future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// An HTTP answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpReply {
    /// The status code.
    pub status: u16,
    /// The JSON body (`null` when it was not JSON).
    pub body: Value,
}

/// How the client reaches HTTP.
pub trait Transport: Send + Sync {
    /// `GET url` with an optional bearer token.
    fn get<'a>(
        &'a self,
        url: &'a str,
        token: Option<&'a str>,
    ) -> BoxFuture<'a, Result<HttpReply, String>>;

    /// `POST url` with a JSON body and an optional bearer token.
    fn post<'a>(
        &'a self,
        url: &'a str,
        token: Option<&'a str>,
        body: &'a Value,
    ) -> BoxFuture<'a, Result<HttpReply, String>>;
}

/// The card paths, newest first.
pub const CARD_PATHS: [&str; 2] = ["/.well-known/agent-card.json", "/.well-known/agent.json"];

/// How often a non-terminal task is polled at most.
const POLL_MAX: Duration = Duration::from_secs(5);

/// A client of one agent.
pub struct Client<'a> {
    transport: &'a dyn Transport,
    token: Option<&'a str>,
}

/// One completed call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Called {
    /// The conversation (the agent's, else the one sent, else new).
    pub context_id: String,
    /// The remote task id.
    pub task_id: Option<String>,
    /// The final state (`None`: the agent answered with a message).
    pub state: Option<TaskState>,
    /// The reply text.
    pub reply: String,
}

impl<'a> Client<'a> {
    /// A client sending `token` as its bearer.
    #[must_use]
    pub fn new(transport: &'a dyn Transport, token: Option<&'a str>) -> Self {
        Self { transport, token }
    }

    /// The card served under `base` (raw JSON and summary).
    ///
    /// # Errors
    ///
    /// The transport's error, or neither card path answering a card.
    pub async fn discover(&self, base: &str) -> Result<(Value, CardSummary), String> {
        let base = base.trim_end_matches('/');
        let mut last = String::new();
        for path in CARD_PATHS {
            let url = format!("{base}{path}");
            let reply = self.transport.get(&url, self.token).await?;
            if reply.status == 200 && reply.body.is_object() {
                let summary = CardSummary::of(&reply.body);
                return Ok((reply.body, summary));
            }
            last = format!("{url}: HTTP {}", reply.status);
        }
        Err(format!("no agent card found ({last})"))
    }

    /// Sends `text` to the agent at `base` in `context_id` and waits until
    /// its task ends, polling a task left running.
    ///
    /// # Errors
    ///
    /// Transport failures, HTTP refusals (401, 429...), JSON-RPC errors, a
    /// task that failed or was rejected.
    pub async fn call(
        &self,
        base: &str,
        text: &str,
        context_id: Option<&str>,
    ) -> Result<Called, String> {
        let (endpoint, dialect) = match self.discover(base).await {
            Ok((_, card)) => {
                let dialect = if card.legacy() {
                    Dialect::V03
                } else {
                    Dialect::V1
                };
                (endpoint(base, card.url.as_deref()), dialect)
            }
            Err(_) => (base.to_owned(), Dialect::V1),
        };
        let send = if dialect == Dialect::V1 {
            "SendMessage"
        } else {
            "message/send"
        };
        let params = wire::send_params(text, context_id, dialect);
        let mut answer = self.rpc(&endpoint, send, params).await?;
        let mut delay = Duration::from_millis(200);
        while let (Some(state), Some(task)) = (answer.state, answer.task_id.clone()) {
            if state.terminal() || state == TaskState::InputRequired {
                break;
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(POLL_MAX);
            let get = if dialect == Dialect::V1 {
                "GetTask"
            } else {
                "tasks/get"
            };
            answer = self.rpc(&endpoint, get, json!({"id": task})).await?;
        }
        if let Some(state @ (TaskState::Failed | TaskState::Rejected | TaskState::Canceled)) =
            answer.state
        {
            return Err(format!(
                "the agent's task ended {}: {}",
                state.name(),
                answer.text
            ));
        }
        Ok(Called {
            context_id: answer
                .context_id
                .or_else(|| context_id.map(str::to_owned))
                .unwrap_or_else(|| ulid::Ulid::generate().to_string()),
            task_id: answer.task_id,
            state: answer.state,
            reply: answer.text,
        })
    }

    async fn rpc(&self, endpoint: &str, method: &str, params: Value) -> Result<Answer, String> {
        let body = json!({
            "jsonrpc": wire::JSONRPC,
            "id": ulid::Ulid::generate().to_string(),
            "method": method,
            "params": params,
        });
        let reply = self.transport.post(endpoint, self.token, &body).await?;
        if let Some(error) = reply.body.get("error").filter(|e| e.is_object()) {
            return Err(format!(
                "the agent answered error {}: {}",
                error["code"],
                error["message"].as_str().unwrap_or_default()
            ));
        }
        match reply.status {
            200 => reply
                .body
                .get("result")
                .map(Answer::of)
                .ok_or_else(|| "the agent's answer has no result".to_owned()),
            401 | 403 => Err(format!(
                "the agent refused the token (HTTP {})",
                reply.status
            )),
            status => Err(format!("the agent answered HTTP {status}")),
        }
    }
}

/// The JSON-RPC endpoint for the agent at `base`: the card's `url` only
/// when it is on the same origin (scheme, host and port) as `base`, else
/// `base` itself. The card is the remote's data, and the caller's grant and
/// bearer token are for `base`'s origin: a card naming another host must
/// not redirect the message and the token there.
fn endpoint(base: &str, card_url: Option<&str>) -> String {
    let origin = |text: &str| url::Url::parse(text).ok().map(|u| u.origin());
    match (card_url, origin(base)) {
        (Some(url), Some(base_origin))
            if base_origin.is_tuple() && origin(url).as_ref() == Some(&base_origin) =>
        {
            url.to_owned()
        }
        _ => base.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Answers the card (whose endpoint is `card_url`), then a working
    /// task, then the completed task; records each request and its token.
    struct Fake {
        calls: Mutex<Vec<String>>,
        card_url: &'static str,
    }

    impl Fake {
        fn new(card_url: &'static str) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                card_url,
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Transport for Fake {
        fn get<'a>(
            &'a self,
            url: &'a str,
            token: Option<&'a str>,
        ) -> BoxFuture<'a, Result<HttpReply, String>> {
            Box::pin(async move {
                let token = token.unwrap_or("-");
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("GET {url} {token}"));
                Ok(HttpReply {
                    status: 200,
                    body: json!({"name": "r", "supportedInterfaces": [{"url": self.card_url, "protocolVersion": "1.0"}]}),
                })
            })
        }

        fn post<'a>(
            &'a self,
            url: &'a str,
            token: Option<&'a str>,
            body: &'a Value,
        ) -> BoxFuture<'a, Result<HttpReply, String>> {
            Box::pin(async move {
                let method = body["method"].as_str().unwrap().to_owned();
                let token = token.unwrap_or("-");
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("POST {url} {method} {token}"));
                let state = if method == "SendMessage" {
                    "TASK_STATE_WORKING"
                } else {
                    "TASK_STATE_COMPLETED"
                };
                let mut task = json!({"id": "t1", "contextId": "c1", "status": {"state": state}});
                if method == "GetTask" {
                    task["artifacts"] = json!([{"parts": [{"text": "done"}]}]);
                }
                Ok(HttpReply {
                    status: 200,
                    body: json!({"jsonrpc": "2.0", "id": body["id"], "result": {"task": task}}),
                })
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_running_task_is_polled_until_it_ends() {
        let fake = Fake::new("http://r/rpc");
        let called = Client::new(&fake, None)
            .call("http://r", "hi", None)
            .await
            .unwrap();
        assert_eq!(called.reply, "done");
        assert_eq!(called.context_id, "c1");
        assert_eq!(
            fake.calls(),
            [
                "GET http://r/.well-known/agent-card.json -",
                "POST http://r/rpc SendMessage -",
                "POST http://r/rpc GetTask -"
            ]
        );
    }

    /// A card naming another origin cannot redirect the message or the
    /// bearer token: every call goes to the granted base URL.
    #[tokio::test(start_paused = true)]
    async fn a_card_on_another_host_is_not_followed() {
        for card_url in [
            "https://attacker.example/x",
            "http://r:8080/rpc",
            "https://r/rpc",
            "not a url",
        ] {
            let fake = Fake::new(card_url);
            Client::new(&fake, Some("sekrit"))
                .call("http://r", "hi", None)
                .await
                .unwrap();
            let calls = fake.calls();
            assert!(
                calls
                    .iter()
                    .filter(|c| c.starts_with("POST"))
                    .all(|c| c.starts_with("POST http://r ")),
                "{card_url}: {calls:?}"
            );
            assert!(
                calls
                    .iter()
                    .all(|c| !c.contains("attacker") && !c.contains(":8080")),
                "{card_url}: {calls:?}"
            );
        }
    }
}
