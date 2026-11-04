//! `tool_call`, the tool-search bridge's runner (spec: tool-system, Tool
//! search progressive disclosure; Tool search listing, ranking and
//! limits).
//!
//! Each entry is resolved only within the session's resolved tool set
//! (exposed or deferred) and dispatched through the full pipeline under its
//! real name, with its own call id (`<call id>#<n>`), so hooks, capability
//! checks, the approval gate, timeouts and the result budget apply to it
//! as to a direct call. A name outside the set (and outside the remote
//! connector namespace, whose tools exist only here) is an unknown tool and
//! is never dispatched.

use std::sync::Arc;

use pmt_core::cancel::CancelToken;
use serde_json::{Value, json};
use tokio::time::Instant;

use super::{AgentLoopCalls, Dispatcher, Executed, ToolCall};
use crate::args;
use crate::result::{Content, Status, ToolResult};
use crate::search::{MAX_BATCH, TOOL_CALL, TOOL_DESCRIBE, TOOL_SEARCH};

/// One requested entry.
struct Entry {
    name: String,
    arguments: Value,
}

impl Dispatcher {
    /// Runs one `tool_call`.
    pub(super) async fn bridge(
        &self,
        call: &ToolCall,
        cancel: &CancelToken,
        agent: Option<&Arc<dyn AgentLoopCalls>>,
    ) -> Executed {
        self.setup
            .observer
            .tool_call(&call.call_id, &call.name, &call.arguments);
        let started = Instant::now();
        let result = match self.entries(call) {
            Ok(entries) => self.run_entries(call, entries, cancel, agent).await,
            Err(message) => ToolResult::error(message),
        };
        Executed {
            tool: self.setup.surface.get(TOOL_CALL).cloned(),
            result,
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        }
    }

    /// The entries of `call`, or the correction to send back.
    fn entries(&self, call: &ToolCall) -> Result<Vec<Entry>, String> {
        let entries = parse(&call.arguments)?;
        let surface = &self.setup.surface;
        if entries.len() > 1
            && let Some(local) = entries
                .iter()
                .find(|e| surface.get(&e.name).is_some() && !is_bridge(&e.name))
        {
            return Err(format!(
                "tool_call batches run only tools found with tool_search; call {} directly. Valid shape: {{\"calls\": [{{\"name\": \"<tool from tool_search>\", \"arguments\": {{...}}}}]}}",
                local.name
            ));
        }
        Ok(entries)
    }

    async fn run_entries(
        &self,
        call: &ToolCall,
        entries: Vec<Entry>,
        cancel: &CancelToken,
        agent: Option<&Arc<dyn AgentLoopCalls>>,
    ) -> ToolResult {
        let mut results = Vec::with_capacity(entries.len());
        for (i, entry) in entries.iter().enumerate() {
            let result = if is_bridge(&entry.name) {
                ToolResult::error(format!(
                    "tool_call cannot run {}; call it directly",
                    entry.name
                ))
            } else if let Some(tool) = self.tool(&entry.name) {
                let inner = ToolCall {
                    call_id: format!("{}#{}", call.call_id, i + 1),
                    name: entry.name.clone(),
                    arguments: entry.arguments.to_string(),
                    turn_id: call.turn_id.clone(),
                };
                let done = self.execute_tool(&inner, Some(tool), cancel, agent).await;
                self.setup.observer.tool_result(
                    &inner.call_id,
                    &inner.name,
                    &done.result,
                    done.duration_ms,
                );
                done.result
            } else {
                ToolResult::error(format!("unknown tool: {}", entry.name))
            };
            results.push(result);
        }
        if results.len() == 1 {
            return results
                .pop()
                .unwrap_or_else(|| ToolResult::error("no result"));
        }
        combined(&entries, results)
    }
}

impl Dispatcher {
    /// An entry's tool: in the session's resolved set, else a remote
    /// (connector) tool.
    fn tool(&self, name: &str) -> Option<Arc<crate::descriptor::RegisteredTool>> {
        let surface = &self.setup.surface;
        surface
            .resolve(name)
            .cloned()
            .or_else(|| surface.remote_tool(name))
    }
}

fn is_bridge(name: &str) -> bool {
    [TOOL_SEARCH, TOOL_DESCRIBE, TOOL_CALL].contains(&name)
}

/// The results of a batch: a JSON array of `{name, status, result}` plus
/// every media part; `error` only when no entry succeeded.
fn combined(entries: &[Entry], results: Vec<ToolResult>) -> ToolResult {
    let any_ok = results.iter().any(|r| r.status.succeeded());
    let mut media = Vec::new();
    let listed: Vec<Value> = entries
        .iter()
        .zip(results)
        .map(|(entry, result)| {
            media.extend(
                result
                    .content
                    .iter()
                    .filter(|c| matches!(c, Content::Media { .. }))
                    .cloned(),
            );
            json!({
                "name": entry.name,
                "status": result.status.as_str(),
                "result": result.text_content(),
            })
        })
        .collect();
    let status = if any_ok { Status::Ok } else { Status::Error };
    let mut result = ToolResult::text(status, Value::Array(listed).to_string());
    result.content.extend(media);
    result
}

/// `{"calls": [...]}`, where `calls` may also be the array as JSON text
/// and each `arguments` may be an object or JSON text of one.
fn parse(raw: &str) -> Result<Vec<Entry>, String> {
    let shape =
        "expected {\"calls\": [{\"name\": \"<tool>\", \"arguments\": {...}}]} with 1 to 10 calls";
    let mut map = args::parse(raw).map_err(|e| format!("invalid arguments for tool_call: {e}"))?;
    let calls = match map.remove("calls") {
        Some(Value::String(text)) => serde_json::from_str(&text)
            .map_err(|e| format!("tool_call: calls is not a JSON array ({e}); {shape}"))?,
        Some(value) => value,
        None => return Err(format!("tool_call: {shape}")),
    };
    let Value::Array(items) = calls else {
        return Err(format!("tool_call: {shape}"));
    };
    if items.is_empty() || items.len() > MAX_BATCH {
        return Err(format!("tool_call: {shape}"));
    }
    items
        .into_iter()
        .map(|item| {
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("tool_call: every call needs a name; {shape}"))?
                .to_owned();
            let arguments = match item.get("arguments") {
                None | Some(Value::Null) => json!({}),
                Some(Value::String(text)) => serde_json::from_str(text)
                    .map_err(|e| format!("tool_call: arguments of {name} are not JSON: {e}"))?,
                Some(value) => value.clone(),
            };
            Ok(Entry { name, arguments })
        })
        .collect()
}
