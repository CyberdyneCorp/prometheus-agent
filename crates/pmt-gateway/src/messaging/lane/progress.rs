//! How a running turn shows in its chat (spec: messaging-gateway, Display
//! policy and per-platform overrides), from `[gateway.display]` with its
//! per-platform overrides:
//!
//! - tool progress: `off`, `new` (a line when the tool changes), `all`,
//!   `verbose` (longer previews and each result) or `log` (nothing in the
//!   chat; each call appended, secret-redacted, to
//!   `<home>/logs/<profile>/tool_calls.log`, rotated at 5 MiB with 3
//!   backups); lines accumulate in one edited bubble or go one per
//!   message, previews cut at `tool_preview_length`;
//! - interim assistant messages: what the model says before a tool call;
//! - long-running notices: one "Working — <n> min" bubble, edited every
//!   180 seconds;
//! - progress cleanup: the bubbles are deleted after a successful final
//!   reply on adapters that can delete (a failed run keeps them);
//! - `/verbose` cycles the chat's mode when `tool_progress_command` is on.
//!
//! Progress messages are ephemeral and sent directly, outside the outbox.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pmt_config::schema::{ChatToolProgress, EffectiveDisplay, PhraseMode, ToolProgressGrouping};
use serde_json::Value;
use tokio::time::Instant;

use super::Lane;
use crate::messaging::adapter::Outgoing;
use crate::messaging::adapter::TaskStatus;

/// How often the long-running bubble is updated.
pub(crate) const WORKING_EVERY: Duration = Duration::from_mins(3);
/// The tool call log's size before it rotates, and the backups kept.
const LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;
const LOG_BACKUPS: usize = 3;
/// Phrases a status phrase file may hold, and their length.
pub(crate) const MAX_PHRASES: usize = 80;
pub(crate) const MAX_PHRASE_CHARS: usize = 160;
/// What the long-running bubble says after `minutes`.
pub(crate) fn working(strings: &pmt_i18n::Strings, minutes: u64) -> String {
    pmt_i18n::t!(strings, "gateway.working", minutes = minutes)
}

/// What the running turn is doing, for a detailed busy acknowledgement.
pub(crate) enum Activity {
    /// A tool runs.
    Tool(String, u64),
    /// The model thinks.
    Thinking(u64),
}

impl Activity {
    /// The activity in the language of `strings`.
    pub(crate) fn render(&self, strings: &pmt_i18n::Strings) -> String {
        match self {
            Self::Tool(tool, minutes) => pmt_i18n::t!(
                strings,
                "gateway.activity_tool",
                tool = tool,
                minutes = minutes
            ),
            Self::Thinking(minutes) => {
                pmt_i18n::t!(strings, "gateway.activity_thinking", minutes = minutes)
            }
        }
    }
}

/// A tool call's arguments as one line of at most `limit` characters.
pub(crate) fn preview(arguments: &Value, limit: usize) -> String {
    let text = match arguments {
        Value::Object(map) if map.len() == 1 => map
            .values()
            .next()
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
            .unwrap_or_default(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= limit {
        line
    } else {
        let mut cut: String = line.chars().take(limit.saturating_sub(1)).collect();
        cut.push('…');
        cut
    }
}

/// The phrases of a status phrase file (blank lines skipped).
pub(crate) fn read_phrases(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The progress of the running turn.
#[derive(Debug)]
pub(crate) struct Progress {
    started: Instant,
    lines: Vec<String>,
    /// The accumulating bubble.
    bubble: Option<String>,
    /// Every progress message sent (for cleanup).
    sent: Vec<String>,
    last_tool: Option<String>,
    /// What the model said last, shown once a tool call follows it.
    interim: Option<String>,
    working: Option<String>,
    working_minutes: u64,
    calls: usize,
    /// Tool calls shown as task cards: call id to title.
    cards: std::collections::HashMap<String, String>,
    /// The live subagent tree of the turn (synchronous children).
    subagents: pmt_core::subagent_tree::TreeLines,
}

impl Progress {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            started: now,
            lines: Vec::new(),
            bubble: None,
            sent: Vec::new(),
            last_tool: None,
            interim: None,
            working: None,
            working_minutes: 0,
            cards: std::collections::HashMap::new(),
            calls: 0,
            subagents: pmt_core::subagent_tree::TreeLines::default(),
        }
    }

    /// What the running turn is doing, for a detailed busy
    /// acknowledgement.
    pub(super) fn activity(&self, now: Instant) -> Activity {
        let minutes = now.duration_since(self.started).as_secs() / 60;
        match &self.last_tool {
            Some(tool) => Activity::Tool(tool.clone(), minutes),
            None => Activity::Thinking(minutes),
        }
    }
}

impl Lane {
    /// The display settings of the conversation's platform now, with the
    /// chat's `/verbose` choice.
    pub(super) fn display(&self) -> EffectiveDisplay {
        let platform = &self.routing.platform;
        let mut display = self.config().map_or_else(
            || pmt_config::schema::GatewayDisplay::default().for_platform(platform),
            |s| s.config().gateway.display.for_platform(platform),
        );
        if let Some(mode) = self.progress_mode {
            display.tool_progress = mode;
        }
        display
    }

    /// A tool call, a tool result or model text of the running turn.
    pub(super) async fn show_progress(&mut self, kind: &str, payload: &Value) {
        if self.running.as_ref().is_none_or(|r| r.quiet) {
            return;
        }
        let display = self.display();
        if kind == "tool.result" && self.finish_card(payload).await {
            return;
        }
        match kind {
            "assistant.message" if display.interim_messages => {
                let text = payload["text"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                if let Some(running) = self.running.as_mut() {
                    running.progress.interim = Some(text).filter(|t| !t.is_empty());
                }
            }
            "tool.call" => self.tool_called(&display, payload).await,
            "subagent.spawned" | "subagent.progress" | "subagent.finished"
                if display.tool_progress != ChatToolProgress::Off
                    && display.tool_progress != ChatToolProgress::Log =>
            {
                let line = self
                    .running
                    .as_mut()
                    .and_then(|r| r.progress.subagents.line(kind, payload));
                if let Some(line) = line {
                    self.progress_line(&display, line).await;
                }
            }
            "tool.result" if display.tool_progress == ChatToolProgress::Verbose => {
                let key = if payload.get("error").is_some_and(|e| !e.is_null()) {
                    "gateway.tool_failed"
                } else {
                    "gateway.tool_done"
                };
                let line = format!("  \u{21b3} {}", self.strings().get(key));
                self.progress_line(&display, line).await;
            }
            _ => {}
        }
    }

    async fn tool_called(&mut self, display: &EffectiveDisplay, payload: &Value) {
        let tool = payload["tool"].as_str().unwrap_or("tool").to_owned();
        let interim = self
            .running
            .as_mut()
            .and_then(|r| r.progress.interim.take());
        if let Some(text) = interim {
            self.send_progress(&text).await;
        }
        let Some(running) = self.running.as_mut() else {
            return;
        };
        let progress = &mut running.progress;
        let changed = progress.last_tool.as_deref() != Some(tool.as_str());
        progress.last_tool = Some(tool.clone());
        progress.calls += 1;
        let arguments = &payload["arguments"];
        if matches!(
            display.tool_progress,
            ChatToolProgress::New | ChatToolProgress::All | ChatToolProgress::Verbose
        ) {
            let id = payload["call_id"].as_str().unwrap_or_default().to_owned();
            let title = format!("{tool}: {}", preview(arguments, display.preview_length));
            if self.task_card(&id, &title, TaskStatus::Running).await {
                if let Some(running) = self.running.as_mut() {
                    running.progress.cards.insert(id, title);
                }
                return;
            }
        }
        match display.tool_progress {
            ChatToolProgress::Off => {}
            ChatToolProgress::Log => self.log_call(&tool, arguments),
            ChatToolProgress::New if !changed => {}
            ChatToolProgress::New | ChatToolProgress::All => {
                let line = format!(
                    "\u{2699} {tool}: {}",
                    preview(arguments, display.preview_length)
                );
                self.progress_line(display, line).await;
            }
            ChatToolProgress::Verbose => {
                let line = format!(
                    "\u{2699} {tool}: {}",
                    preview(arguments, display.preview_length * 4)
                );
                self.progress_line(display, line).await;
            }
        }
    }

    /// A tool result whose call is a task card: the card is marked done
    /// or failed; `false` for calls shown otherwise.
    async fn finish_card(&mut self, payload: &Value) -> bool {
        let id = payload["call_id"].as_str().unwrap_or_default();
        let Some(title) = self
            .running
            .as_mut()
            .and_then(|r| r.progress.cards.remove(id))
        else {
            return false;
        };
        let failed = payload.get("error").is_some_and(|e| !e.is_null());
        let status = if failed {
            TaskStatus::Failed
        } else {
            TaskStatus::Done
        };
        self.task_card(id, &title, status).await;
        true
    }

    /// Shows one progress line: appended to the edited bubble, or as a
    /// message of its own.
    async fn progress_line(&mut self, display: &EffectiveDisplay, line: String) {
        let edits = self
            .shared
            .adapters
            .descriptor(&self.profile, &self.routing.platform)
            .is_some_and(|d| d.supports_edit);
        if display.grouping == ToolProgressGrouping::Separate || !edits {
            self.send_progress(&line).await;
            return;
        }
        let phrase = self.phrase(display);
        let Some(running) = self.running.as_mut() else {
            return;
        };
        running.progress.lines.push(line);
        let text = format!("{phrase}\n{}", running.progress.lines.join("\n"));
        let existing = running.progress.bubble.clone();
        let id = self.put_progress(existing, &text).await;
        if let Some(running) = self.running.as_mut() {
            running.progress.bubble = id;
        }
    }

    /// Edits progress message `existing` to `text`, or sends it when there
    /// is none (or the edit failed); the message's id.
    async fn put_progress(&mut self, existing: Option<String>, text: &str) -> Option<String> {
        if let Some(id) = existing {
            let message = Outgoing::text(
                &self.routing.chat_id,
                self.routing.thread_id.as_deref(),
                text,
            );
            let edited = self
                .shared
                .adapters
                .edit_direct(&self.profile, &self.routing.platform, &id, &message)
                .await;
            match edited {
                Ok(()) => return Some(id),
                Err(err) => tracing::debug!("progress message not edited: {}", err.describe()),
            }
        }
        self.send_progress(text).await
    }

    /// Sends a progress message; its id is kept for cleanup.
    async fn send_progress(&mut self, text: &str) -> Option<String> {
        let message = Outgoing::text(
            &self.routing.chat_id,
            self.routing.thread_id.as_deref(),
            text,
        );
        let sent = self
            .shared
            .adapters
            .send_direct(&self.profile, &self.routing.platform, &message)
            .await;
        match sent {
            Ok(sent) => {
                let id = sent.message_id?;
                if let Some(running) = self.running.as_mut() {
                    running.progress.sent.push(id.clone());
                }
                Some(id)
            }
            Err(err) => {
                tracing::debug!("progress not sent: {}", err.describe());
                None
            }
        }
    }

    /// The working-status phrase for the next bubble update (the
    /// profile's phrase file appends to or replaces the built-in ones).
    fn phrase(&self, display: &EffectiveDisplay) -> String {
        let mut phrases = vec![pmt_i18n::t!(self.strings(), "gateway.working_phrase")];
        if let (Some(spec), Some(scope)) = (&display.status_phrases, self.config())
            && let Ok(text) = std::fs::read_to_string(scope.dir().join(&spec.path))
        {
            let own = read_phrases(&text);
            if spec.mode == PhraseMode::Replace && !own.is_empty() {
                phrases.clear();
            }
            phrases.extend(own.into_iter().take(MAX_PHRASES));
        }
        let calls = self.running.as_ref().map_or(0, |r| r.progress.calls);
        let phrase = &phrases[calls.saturating_sub(1) % phrases.len()];
        phrase.chars().take(MAX_PHRASE_CHARS).collect()
    }

    /// Appends a tool call to the profile's tool call log.
    fn log_call(&self, tool: &str, arguments: &Value) {
        let Some(scope) = self.config() else { return };
        let redacted = scope
            .secrets()
            .redactor()
            .map_or_else(|_| arguments.clone(), |r| r.redact_json(arguments.clone()));
        let line = serde_json::json!({
            "ts": jiff::Timestamp::now().to_string(),
            "session_id": self.session,
            "platform": self.routing.platform,
            "tool": tool,
            "arguments": redacted,
        });
        let path = self
            .shared
            .options
            .home
            .join("logs")
            .join(&self.profile)
            .join("tool_calls.log");
        if let Err(err) = append_rotated(&path, &format!("{line}\n")) {
            tracing::warn!(path = %path.display(), "tool call not logged: {err}");
        }
    }

    /// When the long-running bubble is due next.
    pub(super) fn working_due(&self) -> Option<Instant> {
        let running = self.running.as_ref()?;
        let next = u32::try_from(running.progress.working_minutes / 3 + 1).ok()?;
        Some(running.progress.started + WORKING_EVERY * next)
    }

    /// Sends or edits the long-running bubble when it is due.
    pub(super) async fn check_working(&mut self, now: Instant) {
        if self.working_due().is_none_or(|due| due > now) || !self.display().long_running {
            return;
        }
        let strings = self.strings();
        let Some(running) = self.running.as_mut() else {
            return;
        };
        let minutes = now.duration_since(running.progress.started).as_secs() / 60;
        running.progress.working_minutes = minutes.max(running.progress.working_minutes + 3);
        let text = working(&strings, running.progress.working_minutes);
        let existing = running.progress.working.clone();
        let id = self.put_progress(existing, &text).await;
        if let Some(running) = self.running.as_mut() {
            running.progress.working = id;
        }
    }

    /// After the final reply: deletes the progress messages of a
    /// successful turn where `cleanup_progress` asks for it and the
    /// adapter can delete; a failed run keeps them as breadcrumbs.
    pub(super) async fn clean_up_progress(&self, progress: &Progress, succeeded: bool) {
        if !succeeded || progress.sent.is_empty() || !self.display().cleanup_progress {
            return;
        }
        let Some(slot) = self
            .shared
            .adapters
            .ready(&self.profile, &self.routing.platform)
            .filter(|s| s.descriptor.supports_delete)
        else {
            return;
        };
        for id in &progress.sent {
            let deleted = slot
                .adapter
                .delete(&self.routing.chat_id, self.routing.thread_id.as_deref(), id)
                .await;
            if let Err(err) = deleted {
                tracing::debug!("progress message not deleted: {}", err.describe());
            }
        }
    }

    /// `/verbose`: cycles the chat's tool progress mode when the display
    /// policy lets chats choose.
    pub(super) fn verbose_command(&mut self) {
        let display = self.display();
        if !display.tool_progress_command {
            self.say(self.strings().get("gateway.verbose_locked"));
            return;
        }
        let next = display.tool_progress.next();
        self.progress_mode = Some(next);
        let text = pmt_i18n::t!(self.strings(), "gateway.verbose_set", mode = next.as_str());
        self.say(&text);
    }
}

/// Appends `line` to `path`, rotating it first when it would pass
/// [`LOG_MAX_BYTES`] (`log` -> `log.1` -> ... -> `log.3`).
fn append_rotated(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let size = std::fs::metadata(path).map_or(0, |m| m.len());
    if size > 0 && size + line.len() as u64 > LOG_MAX_BYTES {
        for n in (1..LOG_BACKUPS).rev() {
            let _ = std::fs::rename(backup(path, n), backup(path, n + 1));
        }
        std::fs::rename(path, backup(path, 1))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line.as_bytes())
}

fn backup(path: &Path, n: usize) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preview_is_cut_at_the_limit() {
        let args = serde_json::json!({"command": "ls -la   /very/long/path/that/goes/on"});
        assert_eq!(preview(&args, 10), "ls -la /v…");
        assert_eq!(preview(&args, 200), "ls -la /very/long/path/that/goes/on");
        let two = serde_json::json!({"a": 1, "b": 2});
        assert_eq!(preview(&two, 40), "{\"a\":1,\"b\":2}");
    }

    #[test]
    fn the_log_rotates_keeping_three_backups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool_calls.log");
        let big = "x".repeat(usize::try_from(LOG_MAX_BYTES).unwrap() - 10);
        for _ in 0..5 {
            append_rotated(&path, &big).unwrap();
            append_rotated(&path, "line\n").unwrap();
        }
        assert!(backup(&path, 3).exists());
        assert!(!backup(&path, 4).exists());
    }

    #[test]
    fn working_names_the_minutes() {
        let english = pmt_i18n::Strings::english(pmt_i18n::Surface::Core);
        assert_eq!(working(&english, 6), "Working \u{2014} 6 min");
    }
}
