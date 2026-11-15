//! The text of a kanban notification (spec: kanban, Notifications
//! through the owning profile).

use pmt_kanban::Task;
use pmt_kanban::model::Event;

/// The message a subscription receives for `event` on `task`.
#[must_use]
pub fn text(board: &str, task: &Task, event: &Event) -> String {
    let what = match event.kind.as_str() {
        "completed" => "completed",
        "blocked" => "is blocked",
        "gave_up" => "was given up after repeated failures",
        "review_requested" => "is ready for review",
        other => other,
    };
    let detail = ["summary", "reason"]
        .iter()
        .find_map(|key| event.payload.get(*key).and_then(|v| v.as_str()))
        .filter(|d| !d.trim().is_empty())
        .map(|d| format!(": {}", d.trim()))
        .unwrap_or_default();
    format!(
        "Kanban {board}/{} \"{}\" {what}{detail}",
        task.id, task.title
    )
}
