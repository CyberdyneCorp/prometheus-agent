//! Prefill messages (spec: agent-turn-loop, Prefill messages).
//!
//! `prompt.prefill_messages_file` names a JSON array of `{role, content}`
//! few-shot messages (roles `user` and `assistant`) sent after the system
//! prompt. A session reads the file once, at its start, and freezes the
//! messages with its path and content hash in `state.db`: editing the
//! file changes new sessions only, and the messages are never stored as
//! transcript rows. A missing or malformed file, or another role, fails
//! the session start with `config.invalid` (exit 78).

use std::path::Path;

use pmt_core::error::{Error, codes};
use pmt_providers::request::{Message, Role};
use pmt_store::SessionPrefill;
use serde_json::Value;

const KEY: &str = "prompt.prefill_messages_file";

fn invalid(path: &Path, why: &str) -> Error {
    Error::new(
        &codes::CONFIG_INVALID,
        format!("{KEY}: {}: {why}", path.display()),
    )
}

/// Reads and checks the prefill file `configured` (relative to
/// `profile_dir`).
///
/// # Errors
///
/// `config.invalid` naming the file and what is wrong.
pub fn read(profile_dir: &Path, configured: &str) -> Result<SessionPrefill, Error> {
    let path = profile_dir.join(configured);
    let bytes = std::fs::read(&path).map_err(|e| invalid(&path, &e.to_string()))?;
    let messages: Value =
        serde_json::from_slice(&bytes).map_err(|e| invalid(&path, &format!("not JSON: {e}")))?;
    let items = messages
        .as_array()
        .ok_or_else(|| invalid(&path, "expected an array of {role, content} messages"))?;
    for (i, item) in items.iter().enumerate() {
        let role = item.get("role").and_then(Value::as_str);
        if !matches!(role, Some("user" | "assistant")) {
            return Err(invalid(
                &path,
                &format!("message {i}: role must be user or assistant"),
            ));
        }
        if item.get("content").and_then(Value::as_str).is_none() {
            return Err(invalid(
                &path,
                &format!("message {i}: content must be a string"),
            ));
        }
    }
    Ok(SessionPrefill {
        path: path.display().to_string(),
        sha256: crate::sha256(&bytes),
        messages,
    })
}

/// The wire messages of a frozen prefill.
#[must_use]
pub fn messages(prefill: &SessionPrefill) -> Vec<Message> {
    prefill
        .messages
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let role = match m.get("role")?.as_str()? {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                _ => return None,
            };
            Some(Message::text(role, m.get("content")?.as_str()?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_user_and_assistant_messages_are_accepted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("ok.json"),
            r#"[{"role":"user","content":"2+2?"},{"role":"assistant","content":"4"}]"#,
        )
        .unwrap();
        let ok = read(dir.path(), "ok.json").unwrap();
        assert_eq!(messages(&ok).len(), 2);
        std::fs::write(
            dir.path().join("bad.json"),
            r#"[{"role":"system","content":"x"}]"#,
        )
        .unwrap();
        let err = read(dir.path(), "bad.json").unwrap_err();
        assert_eq!(err.code(), &codes::CONFIG_INVALID);
        assert!(err.message().contains("role must be user or assistant"));
        assert!(read(dir.path(), "missing.json").is_err());
    }
}
