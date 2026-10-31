//! The `memory` tool's arguments, validated whole before anything is
//! written or staged (spec: memory, Memory tool surface; Validation before
//! any write).

use jiff::Timestamp;
use pmt_store::{MemoryKind, MemoryTarget, MemoryTier};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// What one operation does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Creates an entry.
    Add,
    /// Replaces an entry's content (and optionally kind, tier, expiry).
    Patch,
    /// Deletes an entry.
    Remove,
    /// Pins an entry.
    Pin,
    /// Unpins an entry.
    Unpin,
}

impl Action {
    /// The wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Patch => "patch",
            Self::Remove => "remove",
            Self::Pin => "pin",
            Self::Unpin => "unpin",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "add" => Self::Add,
            "patch" => Self::Patch,
            "remove" => Self::Remove,
            "pin" => Self::Pin,
            "unpin" => Self::Unpin,
            _ => return None,
        })
    }

    /// Whether the operation changes an existing entry.
    #[must_use]
    pub const fn selects(self) -> bool {
        !matches!(self, Self::Add)
    }
}

/// One validated operation (also the staged form, with `id` resolved).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Op {
    /// What it does.
    pub action: Action,
    /// The entry, by id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The entry, by a unique substring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    /// New content (trimmed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// New kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<MemoryKind>,
    /// New tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<MemoryTier>,
    /// New expiry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
}

impl Op {
    /// An operation with only `action` set.
    #[must_use]
    pub fn new(action: Action) -> Self {
        Self {
            action,
            id: None,
            old_text: None,
            content: None,
            kind: None,
            tier: None,
            expires_at: None,
        }
    }
}

/// A validated call: one target, its operations in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    /// The target.
    pub target: MemoryTarget,
    /// The operations.
    pub ops: Vec<Op>,
}

const ACTIONS: &str = "add, patch, remove, pin or unpin";

/// Validates `arguments` against the `enabled` targets at `now`.
///
/// # Errors
///
/// The refusal shown to the model; nothing may be written.
pub fn parse(arguments: &Value, enabled: &[MemoryTarget], now: Timestamp) -> Result<Call, String> {
    let object = arguments
        .as_object()
        .ok_or("memory takes an object: {target, operations: [...]}")?;
    let target = parse_target(object.get("target"), enabled)?;
    let raw = operations(object)?;
    let ops = raw
        .iter()
        .enumerate()
        .map(|(i, op)| parse_op(i, op, target, now))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Call { target, ops })
}

fn target_names(enabled: &[MemoryTarget]) -> String {
    enabled
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" or ")
}

fn parse_target(value: Option<&Value>, enabled: &[MemoryTarget]) -> Result<MemoryTarget, String> {
    let names = target_names(enabled);
    let Some(value) = value else {
        return Err(format!("`target` is required: {names}"));
    };
    let target = value
        .as_str()
        .and_then(|t| t.parse::<MemoryTarget>().ok())
        .ok_or_else(|| format!("unknown target {value}: use {names}"))?;
    if !enabled.contains(&target) {
        return Err(format!(
            "the {target} store is disabled ({} = false); use {names}",
            crate::budget::flag(target)
        ));
    }
    Ok(target)
}

/// The raw operations: the `operations` array, or a single operation given
/// at the top level (`{target, action, content}`).
fn operations(object: &Map<String, Value>) -> Result<Vec<Value>, String> {
    match object.get("operations") {
        Some(Value::Array(ops)) if !ops.is_empty() => Ok(ops.clone()),
        Some(_) => Err(format!(
            "`operations` must be a non-empty list of {{action: {ACTIONS}, ...}}"
        )),
        None if object.contains_key("action") => {
            let mut single = object.clone();
            single.remove("target");
            Ok(vec![Value::Object(single)])
        }
        None => Err(format!(
            "`operations` is required: a list of {{action: {ACTIONS}, ...}}"
        )),
    }
}

fn parse_op(i: usize, raw: &Value, target: MemoryTarget, now: Timestamp) -> Result<Op, String> {
    let at = format!("operations[{i}]");
    let object = raw
        .as_object()
        .ok_or_else(|| format!("{at}: expected an object"))?;
    if let Some(nested) = object.get("target")
        && nested.as_str() != Some(target.as_str())
    {
        return Err(format!(
            "{at} names target {nested} but the call's target is \"{target}\": send one call per target"
        ));
    }
    let action = parse_action(&at, object.get("action"))?;
    let mut op = Op::new(action);
    op.id = string(&at, object, "id")?;
    op.old_text = string(&at, object, "old_text")?.filter(|t| !t.trim().is_empty());
    op.content = content(&at, object)?;
    op.kind = enum_field(&at, object, "kind")?;
    op.tier = enum_field(&at, object, "tier")?;
    op.expires_at = expiry(&at, object.get("expires_at"), now)?;
    match action {
        Action::Add if op.content.is_none() => Err(format!("{at}: add needs `content`")),
        Action::Patch if op.content.is_none() => Err(format!(
            "{at}: patch needs `content` (the full new text of the entry)"
        )),
        _ => Ok(op),
    }
}

fn parse_action(at: &str, value: Option<&Value>) -> Result<Action, String> {
    let name = value
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{at}: `action` is required ({ACTIONS})"))?;
    if name == "replace" {
        return Err(format!(
            "{at}: `replace` is not an action; use `patch` with the entry's `id` (or a unique `old_text`) and the new `content`"
        ));
    }
    Action::parse(name).ok_or_else(|| format!("{at}: unknown action '{name}' (use {ACTIONS})"))
}

fn string(at: &str, object: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(other) => Err(format!("{at}: `{key}` must be a string, not {other}")),
    }
}

/// `content`, or its alias `new_text`; trimmed, never empty.
fn content(at: &str, object: &Map<String, Value>) -> Result<Option<String>, String> {
    let content = string(at, object, "content")?;
    let alias = string(at, object, "new_text")?;
    let text = match (content, alias) {
        (Some(a), Some(b)) if a.trim() != b.trim() => {
            return Err(format!("{at}: `content` and `new_text` differ; send one"));
        }
        (Some(text), _) | (None, Some(text)) => text,
        (None, None) => return Ok(None),
    };
    let text = text.trim();
    if text.is_empty() {
        return Err(format!("{at}: `content` is empty"));
    }
    Ok(Some(text.to_owned()))
}

fn enum_field<T: std::str::FromStr>(
    at: &str,
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<T>, String> {
    let Some(text) = string(at, object, key)? else {
        return Ok(None);
    };
    text.parse()
        .map(Some)
        .map_err(|_| format!("{at}: unknown {key} '{text}'{}", allowed(key)))
}

fn allowed(key: &str) -> &'static str {
    match key {
        "kind" => " (fact, preference, instruction, project, person, event)",
        "tier" => " (core, archival)",
        _ => "",
    }
}

fn expiry(at: &str, value: Option<&Value>, now: Timestamp) -> Result<Option<Timestamp>, String> {
    let text = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(text)) => text,
        Some(other) => return Err(format!("{at}: `expires_at` must be a string, not {other}")),
    };
    let when: Timestamp = text.parse().map_err(|_| {
        format!("{at}: `expires_at` '{text}' is not an RFC 3339 timestamp (2026-12-31T00:00:00Z)")
    })?;
    if when <= now {
        return Err(format!("{at}: `expires_at` '{text}' is not in the future"));
    }
    Ok(Some(when))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const BOTH: &[MemoryTarget] = &[MemoryTarget::Memory, MemoryTarget::User];

    fn refused(args: &Value) -> String {
        parse(args, BOTH, Timestamp::now()).unwrap_err()
    }

    #[test]
    fn replace_is_redirected_to_patch() {
        let message = refused(
            &json!({"target": "memory", "operations": [{"action": "replace", "content": "x"}]}),
        );
        assert!(message.contains("use `patch`"), "{message}");
    }

    #[test]
    fn a_mixed_target_batch_names_the_operation() {
        let message = refused(&json!({"target": "memory", "operations": [
            {"action": "add", "content": "a"},
            {"action": "add", "content": "b", "target": "user"}
        ]}));
        assert!(message.starts_with("operations[1]"), "{message}");
    }

    #[test]
    fn every_malformed_field_refuses_the_call() {
        assert!(
            refused(&json!({"operations": [{"action": "add", "content": "a"}]}))
                .contains("`target` is required")
        );
        assert!(refused(&json!({"target": "team", "operations": []})).contains("unknown target"));
        assert!(refused(&json!({"target": "user", "operations": [{"action": "add", "content": "a", "kind": "habit"}]})).contains("unknown kind 'habit'"));
        assert!(refused(&json!({"target": "user", "operations": [{"action": "add", "content": "a", "tier": "warm"}]})).contains("unknown tier"));
        assert!(refused(&json!({"target": "user", "operations": [{"action": "add", "content": "a", "expires_at": "2001-01-01T00:00:00Z"}]})).contains("not in the future"));
        assert!(refused(&json!({"target": "user", "operations": [{"action": "add", "content": "a", "expires_at": "tomorrow"}]})).contains("RFC 3339"));
        assert!(
            refused(&json!({"target": "user", "operations": [{"action": "forget"}]}))
                .contains("unknown action")
        );
        assert!(
            refused(&json!({"target": "user", "operations": [{"action": "patch", "id": "m_x"}]}))
                .contains("patch needs `content`")
        );
        let disabled = parse(
            &json!({"target": "user", "operations": [{"action": "add", "content": "a"}]}),
            &[MemoryTarget::Memory],
            Timestamp::now(),
        )
        .unwrap_err();
        assert!(
            disabled.contains("memory.user_profile_enabled = false"),
            "{disabled}"
        );
    }

    #[test]
    fn new_text_is_an_alias_and_a_top_level_operation_is_accepted() {
        let call = parse(
            &json!({"target": "user", "action": "patch", "id": "m_x", "new_text": "  New.  "}),
            BOTH,
            Timestamp::now(),
        )
        .unwrap();
        assert_eq!(call.ops[0].action, Action::Patch);
        assert_eq!(call.ops[0].content.as_deref(), Some("New."));
    }
}
