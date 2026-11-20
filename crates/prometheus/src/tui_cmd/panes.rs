//! The split panes in the TUI (spec: tui, Split panes for subagents and
//! tool output): `/pane subagents|tools|off`, `F2` and `F3` show them.
//! The subagents pane's rows follow the session's `subagent.*` events
//! (and `subagent.list` when it opens); the selected child's transcript
//! streams through `subagent.subscribe` (its `subagent.event`s), never by
//! polling. The tool pane keeps each tool call's full output.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent};
use pmt_rpc::contracts::{
    SessionRef, SubagentEventKind, SubagentEventPayload, SubagentList, SubagentListing,
    SubagentSubscribe, SubagentSubscribeParams,
};
use pmt_tui::panes::{Call, Child, Streamed, Which};
use serde_json::Value;

use super::screen::{Done, Tui};

impl Tui<'_> {
    /// `F2`/`F3`: shows that pane, or hides it when it is shown.
    pub(super) fn toggle_pane(&mut self, which: Which) {
        self.app.panes.toggle(which);
        self.pane_opened();
    }

    /// `/pane subagents|tools|off`.
    pub(super) fn pane_command(&mut self, args: &str) {
        let which = match args.trim() {
            "subagents" => Some(Which::Subagents),
            "tools" => Some(Which::Tools),
            "off" => None,
            _ => {
                let usage = self.strings().get("tui.pane_usage").to_owned();
                self.notice(usage);
                return;
            }
        };
        self.app.panes.show(which);
        self.pane_opened();
    }

    fn pane_opened(&mut self) {
        if self.app.panes.shown != Some(Which::Subagents) {
            return;
        }
        let params = SessionRef {
            session_id: self.link.session.clone(),
        };
        let client = self.link.client.clone();
        self.background(async move {
            Done::Children(
                client
                    .call_typed::<SubagentListing>(&params)
                    .await
                    .unwrap_or_default(),
            )
        });
        self.subscribe_selected();
    }

    /// `subagent.list` answered: children this screen did not see start.
    pub(super) fn children_listed(&mut self, list: SubagentList) {
        let now = Instant::now();
        for summary in list.subagents {
            if self.app.panes.child_mut(&summary.subagent_id).is_some() {
                continue;
            }
            let elapsed = Duration::from_millis(summary.elapsed_ms.unwrap_or_default());
            let goal = summary.title.unwrap_or_default();
            let started = now.checked_sub(elapsed).unwrap_or(now);
            let mut child = Child::new(&summary.subagent_id, &goal, started);
            if summary.status != "running" {
                child.took = Some(elapsed);
            }
            child.status = summary.status;
            self.app.panes.children.push(child);
        }
        self.subscribe_selected();
    }

    /// Streams the selected child's transcript, once per child.
    fn subscribe_selected(&mut self) {
        if self.app.panes.shown != Some(Which::Subagents) {
            return;
        }
        let Some(child) = self.app.panes.selected_child() else {
            return;
        };
        if !self.subscribed.insert(child.id.clone()) {
            return;
        }
        let params = SubagentSubscribeParams {
            session_id: self.link.session.clone(),
            subagent_id: child.id.clone(),
        };
        let client = self.link.client.clone();
        pmt_core::spawn::spawn(async move {
            // The items arrive as `subagent.event`s; a child the daemon
            // no longer knows keeps its row as the events left it.
            let _ = client.call_typed::<SubagentSubscribe>(&params).await;
        });
    }

    /// `Up`, `Down` and `Esc` while a pane has the focus; `false` for any
    /// other key.
    pub(super) fn pane_key(&mut self, key: &KeyEvent) -> bool {
        let panes = &mut self.app.panes;
        if !panes.focused || panes.shown.is_none() || !key.modifiers.is_empty() {
            return false;
        }
        match key.code {
            KeyCode::Up => panes.move_selection(-1),
            KeyCode::Down => panes.move_selection(1),
            KeyCode::Esc => panes.focused = false,
            _ => return false,
        }
        self.subscribe_selected();
        true
    }

    /// A session event the panes follow: children starting, progressing
    /// and ending, tool calls and their results.
    pub(super) fn pane_event(&mut self, kind: &str, payload: &Value) {
        let text = |key: &str| payload[key].as_str().unwrap_or_default().to_owned();
        let panes = &mut self.app.panes;
        match kind {
            "subagent.spawned" => {
                let id = text("subagent_id");
                if panes.child_mut(&id).is_none() {
                    panes
                        .children
                        .push(Child::new(&id, &text("goal"), Instant::now()));
                }
                self.subscribe_selected();
            }
            "subagent.progress" | "subagent.finished" => {
                let Some(child) = panes.child_mut(&text("subagent_id")) else {
                    return;
                };
                let usage = &payload["usage"];
                let tokens = &usage["tokens"];
                child.tokens = tokens["input"].as_u64().unwrap_or_default()
                    + tokens["output"].as_u64().unwrap_or_default();
                child.cost_usd = usage["cost_usd"].as_f64();
                if kind == "subagent.finished" {
                    child.status = text("status");
                    child.took = payload["duration_ms"].as_u64().map(Duration::from_millis);
                }
            }
            "tool.call" => panes.calls.push(Call {
                id: text("call_id"),
                tool: text("tool"),
                hint: hint(&payload["arguments"]),
                output: None,
            }),
            "tool.result" => {
                let output = payload["output"]
                    .as_str()
                    .or_else(|| payload["error"]["message"].as_str())
                    .unwrap_or_default()
                    .to_owned();
                if let Some(call) = panes.call_mut(&text("call_id")) {
                    call.output = Some(output);
                }
            }
            _ => {}
        }
    }

    /// `subagent.event`: one item of a subscribed child's transcript.
    pub(super) fn on_child_event(&mut self, data: &Value) {
        let Ok(item) = serde_json::from_value::<SubagentEventPayload>(data.clone()) else {
            return;
        };
        let Some(child) = self.app.panes.child_mut(&item.subagent_id) else {
            return;
        };
        let text = item.text.unwrap_or_default();
        child.stream.push(match item.kind {
            SubagentEventKind::Assistant => Streamed::Text(text),
            SubagentEventKind::ToolCall => Streamed::Call(item.tool.unwrap_or_default(), text),
            SubagentEventKind::ToolResult => Streamed::Output(text),
            SubagentEventKind::Finished => {
                let status = item.status.unwrap_or_default();
                child.status.clone_from(&status);
                Streamed::Ended(status)
            }
        });
    }
}

/// What a call works on: its path, command, query or URL.
fn hint(arguments: &Value) -> String {
    ["path", "command", "query", "url", "pattern"]
        .iter()
        .find_map(|key| arguments.get(*key).and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned()
}

/// The transcript's one-line summary of a tool's output (the tool pane
/// holds all of it).
pub(super) fn summary(payload: &Value) -> Option<String> {
    let output = payload["output"]
        .as_str()
        .or_else(|| payload["error"]["message"].as_str())?;
    let first = output.lines().find(|l| !l.trim().is_empty())?;
    let mut line: String = first.chars().take(72).collect();
    if line.len() < first.len() || output.lines().nth(1).is_some() {
        line.push('…');
    }
    Some(format!("↳ {line}"))
}
