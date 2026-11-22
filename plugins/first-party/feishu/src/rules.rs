//! Who may summon the bot from a document comment. Rules live in the
//! plugin's file `comment_rules.json`:
//!
//! ```json
//! {"enabled": true, "policy": "pairing", "allow_from": ["ou_.."],
//!  "documents": {"docx:<token>": {"policy": "allowlist", "allow_from": [..]},
//!                "*": {"enabled": false}}}
//! ```
//!
//! The first tier that exists decides: the exact document rule
//! (`<file_type>:<file_token>`), then the wildcard rule `*`, then the
//! top level; a field the chosen rule leaves out comes from the top
//! level, then the defaults (enabled, `pairing`, nobody listed). A
//! commenter passes when listed in `allow_from`, or, under `pairing`,
//! approved in the pairing store (state key `comment_pairing`, written by
//! `prometheus gateway feishu comment-rules pairing add`). The host's
//! `comment-rules` command resolves rules the same way.

use adapter_kit::host;
use serde_json::Value;

/// The rules file.
pub const FILE: &str = "comment_rules.json";
/// The pairing store's state key.
pub const PAIRING: &str = "comment_pairing";

/// A resolved rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// Comments are answered.
    pub enabled: bool,
    /// `allowlist` or `pairing`.
    pub policy: String,
    /// Listed commenters.
    pub allow_from: Vec<String>,
    /// `exact:<key>`, `wildcard` or `top`.
    pub source: String,
}

fn strings(value: &Value) -> Option<Vec<String>> {
    value.as_array().map(|list| {
        list.iter()
            .filter_map(Value::as_str)
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

fn policy(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_ascii_lowercase)
        .filter(|p| p == "allowlist" || p == "pairing")
}

/// Resolves the rule of `file_type:file_token` in `config`.
#[must_use]
pub fn resolve(config: &Value, file_type: &str, file_token: &str) -> Resolved {
    let exact = format!("{file_type}:{file_token}");
    let documents = &config["documents"];
    let (rule, source) = if documents[&exact].is_object() {
        (&documents[&exact], format!("exact:{exact}"))
    } else if documents["*"].is_object() {
        (&documents["*"], "wildcard".to_owned())
    } else {
        (config, "top".to_owned())
    };
    Resolved {
        enabled: rule["enabled"]
            .as_bool()
            .or(config["enabled"].as_bool())
            .unwrap_or(true),
        policy: policy(&rule["policy"])
            .or_else(|| policy(&config["policy"]))
            .unwrap_or_else(|| "pairing".to_owned()),
        allow_from: strings(&rule["allow_from"])
            .or_else(|| strings(&config["allow_from"]))
            .unwrap_or_default(),
        source,
    }
}

/// Whether `user` passes `rule`, with `approved` the paired users.
#[must_use]
pub fn allows(rule: &Resolved, user: &str, approved: &[String]) -> bool {
    rule.enabled
        && (rule.allow_from.iter().any(|u| u == user)
            || (rule.policy == "pairing" && approved.iter().any(|u| u == user)))
}

/// The rules file's content (`{}` when absent or unreadable).
#[must_use]
pub fn load() -> Value {
    host::read_file(FILE)
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()))
}

/// The users approved in the pairing store.
#[must_use]
pub fn approved() -> Vec<String> {
    host::load(PAIRING)
        .and_then(|v| {
            v["approved"]
                .as_object()
                .map(|m| m.keys().cloned().collect())
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_first_tier_that_exists_decides() {
        let config = json!({
            "policy": "allowlist", "allow_from": ["ou_top"],
            "documents": {
                "docx:d1": {"policy": "allowlist", "allow_from": ["ou_doc"]},
                "*": {"policy": "pairing"},
            },
        });
        let exact = resolve(&config, "docx", "d1");
        assert_eq!(exact.source, "exact:docx:d1");
        assert!(allows(&exact, "ou_doc", &[]));
        assert!(!allows(&exact, "ou_top", &[]), "the document rule wins");
        let wild = resolve(&config, "docx", "d2");
        assert_eq!(wild.source, "wildcard");
        assert_eq!(
            wild.allow_from,
            ["ou_top"],
            "a missing field comes from the top"
        );
        assert!(allows(&wild, "ou_paired", &["ou_paired".into()]));
        let top = resolve(&json!({}), "sheet", "s");
        assert_eq!(
            (top.policy.as_str(), top.source.as_str()),
            ("pairing", "top")
        );
        assert!(!allows(&top, "ou_any", &[]));
    }
}
