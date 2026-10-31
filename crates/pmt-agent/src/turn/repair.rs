//! Transcript repair before each request (spec: agent-turn-loop,
//! Transcript repair before each request).
//!
//! A crash or an interrupt can leave the stored transcript malformed for a
//! provider: consecutive user rows, a tool call without its result, a
//! result without its call, or arguments that are not JSON. The request is
//! built from a repaired copy; the stored rows never change, and each
//! repair is logged at DEBUG with the row id. Repairs are a pure function
//! of the rows, so later requests repeat them byte for byte (the cached
//! prefix survives).

use pmt_providers::request::{Message, Part, Role as WireRole};
use pmt_store::{Message as Row, Role};

use super::transcript;

/// What a call without a stored result answers on the wire.
pub const MISSING_RESULT: &str =
    "[prometheus] No result was recorded for this call (the turn stopped before it finished).";
/// What arguments that are not JSON become on the wire.
pub const MALFORMED_ARGUMENTS: &str = r#"{"prometheus_error":"the arguments were not valid JSON"}"#;

/// The request messages of `rows`, repaired.
#[must_use]
pub fn wire(rows: &[Row]) -> Vec<Message> {
    let mut repair = Repair::default();
    for row in rows {
        repair.row(row);
    }
    repair.close_calls();
    repair.out
}

#[derive(Default)]
struct Repair {
    out: Vec<Message>,
    /// Call ids of the newest assistant row still waiting for a result.
    open: Vec<String>,
}

impl Repair {
    fn row(&mut self, row: &Row) {
        let Some(mut message) = transcript::message(row) else {
            return;
        };
        match row.role {
            Role::Tool => self.result(row, message),
            Role::User => {
                self.close_calls();
                match self.out.last_mut() {
                    Some(last) if last.role == WireRole::User => {
                        tracing::debug!(row = %row.id, "repair: merged consecutive user rows");
                        last.parts.append(&mut message.parts);
                    }
                    _ => self.out.push(message),
                }
            }
            Role::Assistant | Role::System => {
                self.close_calls();
                self.open = fix_calls(row, &mut message);
                self.out.push(message);
            }
        }
    }

    /// A result is kept only right after its call.
    fn result(&mut self, row: &Row, message: Message) {
        let id = row.tool_call_id.as_deref().unwrap_or_default();
        match self.open.iter().position(|open| open == id) {
            Some(at) => {
                self.open.remove(at);
                self.out.push(message);
            }
            None => {
                tracing::debug!(row = %row.id, call = id, "repair: dropped a tool result without its call");
            }
        }
    }

    /// Answers every call still open with a stub result.
    fn close_calls(&mut self) {
        for call_id in std::mem::take(&mut self.open) {
            tracing::debug!(call = %call_id, "repair: stubbed a tool call without its result");
            self.out.push(Message {
                role: WireRole::Tool,
                parts: vec![Part::ToolResult {
                    call_id,
                    content: MISSING_RESULT.to_owned(),
                    is_error: true,
                }],
                cache_breakpoint: false,
            });
        }
    }
}

/// Replaces non-JSON arguments with a stub; returns the call ids.
fn fix_calls(row: &Row, message: &mut Message) -> Vec<String> {
    let mut ids = Vec::new();
    for part in &mut message.parts {
        if let Part::ToolCall { id, arguments, .. } = part {
            if serde_json::from_str::<serde_json::Value>(arguments).is_err() {
                tracing::debug!(row = %row.id, call = %id, "repair: malformed tool-call arguments stubbed");
                MALFORMED_ARGUMENTS.clone_into(arguments);
            }
            ids.push(id.clone());
        }
    }
    ids
}
