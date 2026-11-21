//! Access policies shared by catalog adapters: `dm_policy` and
//! `group_policy` (`open`, `allowlist`, `pairing`, `disabled`) over an
//! allowlist. An empty allowlist under `allowlist` admits nobody.

use serde_json::Value;

/// A policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Everyone.
    Open,
    /// Only listed ids.
    Allowlist,
    /// Unknown senders go through the gateway's pairing.
    Pairing,
    /// Nobody.
    Disabled,
}

/// What the adapter does with a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Admit it, the sender is allowed.
    Allow,
    /// Drop it.
    Deny,
    /// Admit it and let the gateway decide (pairing, its own allowlists).
    Unknown,
}

impl Verdict {
    /// The `authorize-sender` answer.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Unknown => "unknown",
        }
    }
}

impl Policy {
    /// Parses option `key` of `options`, `default` when unset.
    ///
    /// # Errors
    ///
    /// A value that is not a policy.
    pub fn option(options: &Value, key: &str, default: Self) -> Result<Self, String> {
        match options[key].as_str() {
            None => Ok(default),
            Some("open") => Ok(Self::Open),
            Some("allowlist") => Ok(Self::Allowlist),
            Some("pairing") => Ok(Self::Pairing),
            Some("disabled") => Ok(Self::Disabled),
            Some(other) => Err(format!(
                "options.{key}: '{other}' is not one of open, allowlist, pairing, disabled"
            )),
        }
    }

    /// The verdict for `id` with `allowlist`.
    #[must_use]
    pub fn verdict(self, allowlist: &[String], id: &str) -> Verdict {
        let listed = allowlist.iter().any(|a| a == id || a == "*");
        match self {
            Self::Open => Verdict::Allow,
            Self::Disabled => Verdict::Deny,
            Self::Allowlist if listed => Verdict::Allow,
            Self::Allowlist => Verdict::Deny,
            Self::Pairing if listed => Verdict::Allow,
            Self::Pairing => Verdict::Unknown,
        }
    }
}

/// A list of strings at option `key` (empty when unset).
#[must_use]
pub fn strings(options: &Value, key: &str) -> Vec<String> {
    options[key]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_empty_allowlist_admits_nobody_and_pairing_defers() {
        assert_eq!(Policy::Allowlist.verdict(&[], "a"), Verdict::Deny);
        assert_eq!(
            Policy::Allowlist.verdict(&["a".into()], "a"),
            Verdict::Allow
        );
        assert_eq!(Policy::Pairing.verdict(&[], "a"), Verdict::Unknown);
        assert_eq!(Policy::Disabled.verdict(&["a".into()], "a"), Verdict::Deny);
    }

    #[test]
    fn a_bad_policy_names_the_option() {
        let err =
            Policy::option(&json!({"dm_policy": "maybe"}), "dm_policy", Policy::Open).unwrap_err();
        assert!(err.contains("options.dm_policy"), "{err}");
    }
}
