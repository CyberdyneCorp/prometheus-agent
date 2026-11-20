//! One Codex turn as the normalised event stream: agent message and
//! reasoning deltas become text and reasoning, finished commands, file
//! changes and tool calls become [`crate::StreamEvent::ExternalCall`]s,
//! token usage is reported once, and `turn/completed` ends the stream.
//! Codex's approval requests are answered by the session's gate.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::OwnedMutexGuard;

use super::server::{Server, failed, of_session};
use super::{RuntimeApprovals, RuntimeAsk};
use crate::classify::ProviderError;
use crate::event::{ExternalCall, Normalizer, StopReason, UsageReport};
use crate::request::Request;
use crate::transport::Limits;

/// A running Codex turn.
pub(crate) struct CodexFeed {
    server: OwnedMutexGuard<Server>,
    approvals: Option<Arc<dyn RuntimeApprovals>>,
    session: Option<String>,
    done: bool,
}

impl std::fmt::Debug for CodexFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexFeed")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl CodexFeed {
    /// Starts `request` as a turn on the session's server.
    pub(crate) async fn start(
        program: &std::path::Path,
        request: &Request,
        session: Option<String>,
        approvals: Option<Arc<dyn RuntimeApprovals>>,
        limits: Limits,
    ) -> Result<Self, ProviderError> {
        let workdir = session
            .as_deref()
            .zip(approvals.as_deref())
            .and_then(|(session, approvals)| approvals.workdir(session));
        let server = of_session(program, session.as_deref(), workdir);
        let mut server = server.lock_owned().await;
        limits.within(server.start_turn(request)).await??;
        Ok(Self {
            server,
            approvals,
            session,
            done: false,
        })
    }

    /// Streams the next frame into `out`.
    pub(crate) async fn step(
        &mut self,
        out: &mut Normalizer,
        limits: Limits,
    ) -> Result<(), ProviderError> {
        if self.done {
            out.finish(None);
            return Ok(());
        }
        let frame = limits.within(self.server.next_frame()).await??;
        let Some(method) = frame["method"].as_str() else {
            return Ok(());
        };
        let params = &frame["params"];
        if let Some(id) = frame.get("id") {
            return self.answer(id.clone(), method, params).await;
        }
        match method {
            "item/agentMessage/delta" => out.text(params["delta"].as_str().unwrap_or_default()),
            "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
                out.reasoning(params["delta"].as_str().unwrap_or_default());
            }
            "item/completed" => {
                if let Some(call) = projected(&params["item"]) {
                    out.external_call(call);
                }
            }
            "thread/tokenUsage/updated" => out.usage(usage(&params["tokenUsage"]["last"])),
            "turn/completed" => return self.completed(&params["turn"], out),
            _ => {}
        }
        Ok(())
    }

    fn completed(&mut self, turn: &Value, out: &mut Normalizer) -> Result<(), ProviderError> {
        self.done = true;
        match turn["status"].as_str() {
            Some("failed") => Err(failed(
                turn["error"]["message"]
                    .as_str()
                    .unwrap_or("the turn failed"),
            )),
            Some("interrupted") => {
                out.finish(Some(StopReason::Error));
                Ok(())
            }
            _ => {
                out.finish(Some(StopReason::EndTurn));
                Ok(())
            }
        }
    }

    /// Answers a server request: command and file-change approvals go to
    /// the session's gate (declined without one); anything else is not
    /// offered.
    async fn answer(
        &mut self,
        id: Value,
        method: &str,
        params: &Value,
    ) -> Result<(), ProviderError> {
        let ask = match method {
            "item/commandExecution/requestApproval" => {
                Some(("terminal", command_arguments(params)))
            }
            "item/fileChange/requestApproval" => Some(("patch", change_arguments(params))),
            _ => None,
        };
        let reply = match ask {
            Some((tool, arguments)) => {
                let ask = RuntimeAsk {
                    session: self.session.clone(),
                    call_id: params["itemId"].as_str().unwrap_or_default().to_owned(),
                    tool,
                    arguments,
                };
                let allowed = match &self.approvals {
                    Some(approvals) => approvals.ask(ask).await,
                    None => false,
                };
                let decision = if allowed { "accept" } else { "decline" };
                json!({"jsonrpc": "2.0", "id": id, "result": {"decision": decision}})
            }
            None => json!({"jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": format!("{method} is not offered")}}),
        };
        self.server.send(&reply).await
    }
}

fn command_arguments(params: &Value) -> Value {
    let mut arguments = json!({"command": command_text(&params["command"])});
    if let Some(cwd) = params["cwd"].as_str() {
        arguments["workdir"] = cwd.into();
    }
    if let Some(reason) = params["reason"].as_str() {
        arguments["reason"] = reason.into();
    }
    arguments
}

fn change_arguments(params: &Value) -> Value {
    let mut arguments = json!({});
    for key in ["reason", "grantRoot"] {
        if let Some(value) = params[key].as_str() {
            arguments[key] = value.into();
        }
    }
    arguments
}

/// A command as text (Codex sends a string or an argv array).
fn command_text(command: &Value) -> String {
    match command {
        Value::Array(argv) => argv
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        other => other.as_str().unwrap_or_default().to_owned(),
    }
}

/// A finished Codex item as the tool call it is recorded under; messages,
/// reasoning and the user's own input are not actions.
fn projected(item: &Value) -> Option<ExternalCall> {
    let id = item["id"].as_str().unwrap_or_default().to_owned();
    let failed = matches!(item["status"].as_str(), Some("failed" | "declined"));
    let (name, arguments, output, ok) = match item["type"].as_str()? {
        "agentMessage" | "reasoning" | "userMessage" => return None,
        "commandExecution" => {
            let ok = !failed && item["exitCode"].as_i64().is_none_or(|code| code == 0);
            let mut arguments = json!({"command": command_text(&item["command"])});
            if let Some(cwd) = item["cwd"].as_str() {
                arguments["workdir"] = cwd.into();
            }
            let output = item["aggregatedOutput"].as_str().unwrap_or_default();
            ("terminal".to_owned(), arguments, output.to_owned(), ok)
        }
        "fileChange" => {
            let changes = item["changes"].as_array().cloned().unwrap_or_default();
            let paths: Vec<Value> = changes.iter().map(|c| c["path"].clone()).collect();
            let diff = changes
                .iter()
                .filter_map(|c| c["diff"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            ("patch".to_owned(), json!({"paths": paths}), diff, !failed)
        }
        "mcpToolCall" => {
            let name = format!(
                "mcp_{}_{}",
                item["server"].as_str().unwrap_or("server"),
                item["tool"].as_str().unwrap_or("tool")
            );
            let output = match item.get("error").filter(|e| !e.is_null()) {
                Some(error) => error["message"].as_str().unwrap_or("failed").to_owned(),
                None => item["result"].to_string(),
            };
            let ok = !failed && item.get("error").is_none_or(Value::is_null);
            (name, item["arguments"].clone(), output, ok)
        }
        "webSearch" => {
            let query = item["query"].clone();
            (
                "web_search".to_owned(),
                json!({"query": query}),
                String::new(),
                !failed,
            )
        }
        other => (
            format!("codex_{other}"),
            item.clone(),
            String::new(),
            !failed,
        ),
    };
    Some(ExternalCall {
        id,
        name,
        arguments,
        output,
        ok,
    })
}

/// Codex's token counts (`inputTokens` includes the cached ones).
fn usage(last: &Value) -> UsageReport {
    let count = |key: &str| last[key].as_u64();
    let cached = count("cachedInputTokens").unwrap_or(0);
    UsageReport {
        input: count("inputTokens").map(|input| input.saturating_sub(cached)),
        output: count("outputTokens"),
        cache_read: Some(cached),
        cache_write: None,
        reasoning: count("reasoningOutputTokens"),
        cost_usd: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finished_items_become_the_calls_they_correspond_to() {
        let command = json!({"type": "commandExecution", "id": "c1", "command": "ls -la",
            "cwd": "/w", "status": "completed", "exitCode": 0, "aggregatedOutput": "a\nb"});
        let call = projected(&command).unwrap();
        assert_eq!((call.name.as_str(), call.ok), ("terminal", true));
        assert_eq!(
            call.arguments,
            json!({"command": "ls -la", "workdir": "/w"})
        );
        assert_eq!(call.output, "a\nb");
        let patch = json!({"type": "fileChange", "id": "p1", "status": "completed",
            "changes": [{"path": "src/a.rs", "kind": {"type": "update"}, "diff": "@@ -1 +1 @@"}]});
        let call = projected(&patch).unwrap();
        assert_eq!(call.name, "patch");
        assert_eq!(call.arguments, json!({"paths": ["src/a.rs"]}));
        let failed = json!({"type": "commandExecution", "id": "c2", "command": ["false"],
            "status": "failed", "exitCode": 1});
        assert!(!projected(&failed).unwrap().ok);
        assert!(projected(&json!({"type": "agentMessage", "id": "m", "text": "hi"})).is_none());
    }
}
