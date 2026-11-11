//! Session transcripts from other agent CLIs (spec: hermes-migration,
//! Session transcripts from other agent CLIs): a Claude Code session log
//! (`~/.claude/projects/**/*.jsonl`) or a Codex CLI rollout
//! (`~/.codex/sessions/**/rollout-*.jsonl`), read only, becomes one
//! session with the ordered user and assistant messages. Tool activity
//! is condensed to `[ran tool: <name>]` notes in the assistant turn;
//! system prompts, injected context, reasoning and raw tool output are
//! left out. The foreign session id is kept as a legacy id, so importing
//! the same log again returns the existing session.

use std::path::{Path, PathBuf};

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use pmt_core::id::{MessageId, SessionId};
use pmt_store::Store;
use pmt_store::legacy::LegacyLink;
use pmt_store::messages::Message;
use pmt_store::model::{Archived, Origin, Role, Source, TitleSource};
use pmt_store::portable::Portable;
use serde::Serialize;
use serde_json::Value;

/// A CLI whose logs can be imported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cli {
    /// Claude Code.
    ClaudeCode,
    /// Codex CLI.
    Codex,
}

impl Cli {
    /// The `--from` name and source tag suffix.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }

    /// The display name in titles.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex CLI",
        }
    }

    /// The directory its logs are under, below its config directory.
    #[must_use]
    pub fn logs_dir(self, config_dir: &Path) -> PathBuf {
        match self {
            Self::ClaudeCode => config_dir.join("projects"),
            Self::Codex => config_dir.join("sessions"),
        }
    }

    fn is_log(self, path: &Path) -> bool {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        name.ends_with(".jsonl") && (self == Self::ClaudeCode || name.starts_with("rollout-"))
    }
}

/// One conversation read from a log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    /// The id the CLI gave the session.
    pub foreign_id: String,
    /// The ordered user and assistant messages.
    pub messages: Vec<(Role, String)>,
}

impl Transcript {
    /// The first user message (the title and the picker preview).
    #[must_use]
    pub fn first_user(&self) -> Option<&str> {
        self.messages
            .iter()
            .find(|(r, _)| *r == Role::User)
            .map(|(_, t)| t.as_str())
    }
}

/// Text that the CLI injected rather than the user typed.
fn injected(text: &str) -> bool {
    let t = text.trim_start();
    [
        "<system-reminder>",
        "<command-",
        "<local-command-",
        "<environment_context>",
        "<user_instructions>",
        "# AGENTS.md instructions",
        "Caveat: The messages below",
    ]
    .iter()
    .any(|p| t.starts_with(p))
}

/// The plain text of a message `content` (a string or a block list);
/// tool uses become notes, everything else is dropped.
fn text_of(content: &Value, notes: &mut Vec<String>) -> String {
    match content {
        Value::String(s) if !injected(s) => s.trim().to_owned(),
        Value::Array(blocks) => {
            let mut parts = Vec::new();
            for b in blocks {
                let kind = b.get("type").and_then(Value::as_str).unwrap_or_default();
                match kind {
                    "text" | "input_text" | "output_text" => {
                        let t = b.get("text").and_then(Value::as_str).unwrap_or_default();
                        if !injected(t) && !t.trim().is_empty() {
                            parts.push(t.trim().to_owned());
                        }
                    }
                    "tool_use" => {
                        let name = b.get("name").and_then(Value::as_str).unwrap_or("tool");
                        notes.push(format!("[ran tool: {name}]"));
                    }
                    _ => {}
                }
            }
            parts.join("\n\n")
        }
        _ => String::new(),
    }
}

/// Adds a message, merging consecutive assistant parts into one turn.
fn push(messages: &mut Vec<(Role, String)>, role: Role, text: String) {
    if text.is_empty() {
        return;
    }
    match messages.last_mut() {
        Some((Role::Assistant, last)) if role == Role::Assistant => {
            last.push_str("\n\n");
            last.push_str(&text);
        }
        _ => messages.push((role, text)),
    }
}

fn assistant_turn(text: String, notes: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !text.is_empty() {
        parts.push(text);
    }
    if !notes.is_empty() {
        parts.push(notes.join("\n"));
    }
    parts.join("\n\n")
}

fn claude_line(line: &Value, messages: &mut Vec<(Role, String)>) {
    let kind = line.get("type").and_then(Value::as_str).unwrap_or_default();
    let meta = line.get("isMeta").and_then(Value::as_bool) == Some(true);
    let Some(content) = line.pointer("/message/content") else {
        return;
    };
    let mut notes = Vec::new();
    match kind {
        "user" if !meta => {
            // A user line holding only tool results is raw tool output.
            let text = text_of(content, &mut notes);
            push(messages, Role::User, text);
        }
        "assistant" => {
            let text = text_of(content, &mut notes);
            push(messages, Role::Assistant, assistant_turn(text, &notes));
        }
        _ => {}
    }
}

fn codex_line(line: &Value, messages: &mut Vec<(Role, String)>, id: &mut Option<String>) {
    let payload = line.get("payload").unwrap_or(&Value::Null);
    match line.get("type").and_then(Value::as_str).unwrap_or_default() {
        "session_meta" => {
            if let Some(found) = payload.get("id").and_then(Value::as_str) {
                *id = Some(found.to_owned());
            }
        }
        "response_item" => {
            let mut notes = Vec::new();
            match (
                payload.get("type").and_then(Value::as_str),
                payload.get("role").and_then(Value::as_str),
            ) {
                (Some("message"), Some("user")) => {
                    let text = text_of(payload.get("content").unwrap_or(&Value::Null), &mut notes);
                    push(messages, Role::User, text);
                }
                (Some("message"), Some("assistant")) => {
                    let text = text_of(payload.get("content").unwrap_or(&Value::Null), &mut notes);
                    push(messages, Role::Assistant, text);
                }
                (Some("function_call" | "custom_tool_call" | "local_shell_call"), _) => {
                    let name = payload
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("shell");
                    push(messages, Role::Assistant, format!("[ran tool: {name}]"));
                }
                _ => {}
            }
        }
        _ => {}
    }
}

/// Reads the log at `path`.
///
/// # Errors
///
/// `migrate.source_missing` when it cannot be read, `migrate.component`
/// when it holds no conversation.
pub fn read(cli: Cli, path: &Path) -> Result<Transcript, Error> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::io(&codes::MIGRATE_SOURCE_MISSING, path, &e))?;
    let mut messages = Vec::new();
    let mut id = None;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match cli {
            Cli::ClaudeCode => {
                if id.is_none() {
                    id = value
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
                claude_line(&value, &mut messages);
            }
            Cli::Codex => codex_line(&value, &mut messages, &mut id),
        }
    }
    if messages.is_empty() {
        return Err(Error::new(
            &codes::MIGRATE_COMPONENT,
            format!("{} holds no {} conversation", path.display(), cli.label()),
        ));
    }
    let foreign_id = id.unwrap_or_else(|| {
        path.file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    });
    Ok(Transcript {
        foreign_id,
        messages,
    })
}

/// The logs under `dir`, newest first.
#[must_use]
pub fn logs(cli: Cli, dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.is_dir() {
                stack.push(path);
            } else if cli.is_log(&path) {
                let modified = e.metadata().and_then(|m| m.modified()).ok();
                found.push((modified, path));
            }
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    found.into_iter().map(|(_, p)| p).collect()
}

/// The import of one log.
#[derive(Debug, Clone, Serialize)]
pub struct Imported {
    /// The Prometheus session.
    pub session_id: String,
    /// A new session was created (false: the log was imported before).
    pub created: bool,
    /// Messages carried.
    pub messages: usize,
}

/// Imports `transcript` into `store`, or returns the session a previous
/// import created.
///
/// # Errors
///
/// Store errors.
pub fn import(store: &Store, cli: Cli, transcript: &Transcript) -> Result<Imported, Error> {
    if let Some(id) = store.legacy_session(cli.id(), &transcript.foreign_id)? {
        return Ok(Imported {
            session_id: id.to_string(),
            created: false,
            messages: transcript.messages.len(),
        });
    }
    let id = SessionId::new();
    let now = Timestamp::now();
    let first = transcript.first_user().unwrap_or_default();
    let line = first.lines().next().unwrap_or_default();
    let short: String = line.chars().take(60).collect();
    let messages: Vec<Message> = transcript
        .messages
        .iter()
        .map(|(role, text)| Message {
            id: MessageId::new(),
            session_id: id,
            seq: 0,
            role: *role,
            content: Some(text.clone()),
            tool_calls: None,
            reasoning: None,
            provider_payload: None,
            tool_call_id: None,
            tool_name: None,
            metadata: None,
            content_hash: String::new(),
            created_at: now,
            active: true,
            compacted: false,
            compaction_epoch: None,
            content_ref: None,
        })
        .collect();
    let session = pmt_store::Session {
        id,
        profile_id: String::new(),
        source: Source::Import,
        source_tag: Some(cli.id().to_owned()),
        origin: Origin::None,
        model: None,
        provider: None,
        cwd: None,
        git_repo_root: None,
        git_branch: None,
        title: Some(format!("Imported from {}: {short}", cli.label())),
        title_source: Some(TitleSource::Derived),
        started_at: now,
        last_activity_at: now,
        ended_at: None,
        end_reason: None,
        pinned: false,
        archived: Archived::None,
        hidden: false,
        bot_chat: false,
        system_prompt_hash: None,
        parent_session_id: None,
        lineage_kind: None,
        parent_deleted: false,
        root_session_id: id,
        message_count: 0,
        tool_call_count: 0,
        compaction_count: 0,
        rewind_count: 0,
        trajectory_purged_at: None,
        project_id: None,
    };
    let count = messages.len();
    store.import_linked(
        &[Portable {
            session,
            messages,
            usage: Vec::new(),
            blob_refs: Vec::new(),
            session_blobs: Vec::new(),
        }],
        &[LegacyLink {
            source: cli.id().to_owned(),
            legacy_id: transcript.foreign_id.clone(),
            session: id,
            messages: count as u64,
        }],
    )?;
    Ok(Imported {
        session_id: id.to_string(),
        created: true,
        messages: count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_tool_output_and_injected_context_are_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("s.jsonl");
        let output = "line\n".repeat(5000);
        let lines = [
            serde_json::json!({"type": "user", "sessionId": "abc", "isMeta": true, "message": {"role": "user", "content": "Caveat: The messages below were generated"}}),
            serde_json::json!({"type": "user", "sessionId": "abc", "message": {"role": "user", "content": "list the files"}}),
            serde_json::json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "thinking", "thinking": "hmm"}, {"type": "text", "text": "Listing."}, {"type": "tool_use", "name": "Bash", "input": {"command": "ls"}}]}}),
            serde_json::json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "content": output}]}}),
            serde_json::json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "Two files."}]}}),
        ];
        let text: Vec<String> = lines.iter().map(Value::to_string).collect();
        std::fs::write(&log, text.join("\n")).unwrap();
        let t = read(Cli::ClaudeCode, &log).unwrap();
        assert_eq!(t.foreign_id, "abc");
        assert_eq!(t.messages.len(), 2, "{:?}", t.messages);
        assert_eq!(t.messages[0], (Role::User, "list the files".to_owned()));
        let answer = &t.messages[1].1;
        assert!(
            answer.contains("[ran tool: Bash]") && answer.contains("Two files."),
            "{answer}"
        );
        assert!(
            !answer.contains("line\n") && !answer.contains("hmm"),
            "{answer}"
        );
    }

    #[test]
    fn codex_rollouts_skip_developer_and_environment_messages() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("rollout-2026-01-01-x.jsonl");
        let lines = [
            serde_json::json!({"type": "session_meta", "payload": {"id": "019a-codex"}}),
            serde_json::json!({"type": "response_item", "payload": {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "system rules"}]}}),
            serde_json::json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "<environment_context>cwd</environment_context>"}]}}),
            serde_json::json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "fix the test"}]}}),
            serde_json::json!({"type": "response_item", "payload": {"type": "reasoning", "summary": []}}),
            serde_json::json!({"type": "response_item", "payload": {"type": "function_call", "name": "shell", "arguments": "{}"}}),
            serde_json::json!({"type": "response_item", "payload": {"type": "function_call_output", "output": "raw"}}),
            serde_json::json!({"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Fixed."}]}}),
        ];
        let text: Vec<String> = lines.iter().map(Value::to_string).collect();
        std::fs::write(&log, text.join("\n")).unwrap();
        let t = read(Cli::Codex, &log).unwrap();
        assert_eq!(t.foreign_id, "019a-codex");
        assert_eq!(
            t.messages,
            [
                (Role::User, "fix the test".to_owned()),
                (Role::Assistant, "[ran tool: shell]\n\nFixed.".to_owned())
            ]
        );
    }
}
