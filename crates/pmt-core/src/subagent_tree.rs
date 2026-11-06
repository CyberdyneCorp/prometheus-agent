//! Lines of the live subagent spawn tree (spec: subagent-delegation, Live
//! child progress streaming): every surface (line mode, the TUI, messaging
//! chats) renders a session's `subagent.spawned`, `subagent.progress` and
//! `subagent.finished` events as these lines, never the events themselves,
//! which also never reach a model's context.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value;

/// Longest activity summary.
pub const MAX_ACTIVITY_CHARS: usize = 120;

/// `text` as a one-line summary of at most [`MAX_ACTIVITY_CHARS`].
#[must_use]
pub fn one_line(text: &str) -> String {
    let line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= MAX_ACTIVITY_CHARS {
        return line;
    }
    let mut cut: String = line.chars().take(MAX_ACTIVITY_CHARS - 1).collect();
    cut.push('…');
    cut
}

/// One line of the live spawn tree for a `subagent.*` event (`kind` and
/// its payload), indented by depth; `None` for other events. Surfaces
/// show these instead of the events themselves, which never reach a
/// model's context.
#[must_use]
pub fn tree_line(kind: &str, payload: &Value) -> Option<String> {
    let text = |key: &str| payload[key].as_str().unwrap_or_default().to_owned();
    let depth = payload["depth"].as_u64().unwrap_or(1);
    let indent = "  ".repeat(usize::try_from(depth.saturating_sub(1)).unwrap_or(0));
    let id = text("subagent_id");
    match kind {
        "subagent.spawned" => Some(format!("{indent}↳ {id} started: {}", text("goal"))),
        "subagent.progress" => Some(format!(
            "{indent}  {id} · step {} · {}",
            payload["iteration"].as_u64().unwrap_or(0),
            text("activity")
        )),
        "subagent.finished" => Some(format!(
            "{indent}✓ {id} {}: {}",
            text("status"),
            one_line(&text("summary"))
        )),
        _ => None,
    }
}

/// The least time between two progress lines of one child on a surface:
/// progress events arrive up to four a second per child, far faster than
/// a scrolling transcript can be read.
pub const LINE_EVERY: Duration = Duration::from_secs(3);

/// Turns a session's `subagent.*` events into lines of the live spawn
/// tree for a scrolling surface (line mode, the TUI transcript): every
/// spawn and finish, and a child's progress when its activity changed and
/// [`LINE_EVERY`] passed since its last line.
#[derive(Debug, Default)]
pub struct TreeLines {
    shown: HashMap<String, (Instant, String)>,
    /// Children that finished: a late progress event (the pacer's last
    /// state) is not shown after their final line.
    finished: std::collections::HashSet<String>,
}

impl TreeLines {
    /// The line for the `kind` event with `payload`, if one is due.
    pub fn line(&mut self, kind: &str, payload: &Value) -> Option<String> {
        let id = payload["subagent_id"].as_str().unwrap_or_default();
        match kind {
            "subagent.progress" if self.finished.contains(id) => return None,
            "subagent.progress" => {
                let activity = payload["activity"].as_str().unwrap_or_default();
                let now = Instant::now();
                let due = self.shown.get(id).is_none_or(|(at, last)| {
                    last != activity && now.duration_since(*at) >= LINE_EVERY
                });
                if !due || activity.is_empty() {
                    return None;
                }
                self.shown.insert(id.to_owned(), (now, activity.to_owned()));
            }
            "subagent.finished" => {
                self.shown.remove(id);
                self.finished.insert(id.to_owned());
            }
            _ => {}
        }
        tree_line(kind, payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every surface renders the live tree: spawns and finishes always,
    /// progress only when it changed and not more than once per
    /// [`LINE_EVERY`] per child.
    #[test]
    fn tree_lines_show_changes_without_flooding() {
        let mut lines = TreeLines::default();
        let event = |activity: &str| serde_json::json!({"subagent_id": "sa-1", "depth": 1, "iteration": 2, "activity": activity, "goal": "g", "status": "completed", "summary": "s"});
        assert!(lines.line("subagent.spawned", &event("")).is_some());
        let first = lines.line("subagent.progress", &event("read_file src/lib.rs"));
        assert!(first.unwrap().contains("read_file src/lib.rs"));
        assert_eq!(lines.line("subagent.progress", &event("terminal ls")), None);
        assert!(lines.line("subagent.finished", &event("")).is_some());
        assert_eq!(
            lines.line("subagent.progress", &event("model call 3")),
            None
        );
    }

    #[test]
    fn activity_is_one_short_line() {
        assert_eq!(one_line("read\nfile  src/lib.rs"), "read file src/lib.rs");
        assert_eq!(one_line(&"x".repeat(300)).chars().count(), 120);
    }
}
