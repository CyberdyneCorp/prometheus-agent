//! Model-facing connector tool names: `connectors__<connector>__<tool>`.
//!
//! The gateway's vendor slugs are usually the connector and the action in
//! upper case (`GMAIL_SEND_EMAIL`); the model sees the prefix stripped and
//! the rest lower-cased (`connectors__gmail__send_email`). A slug without
//! the prefix keeps its spelling. Names are composed and parsed only here.

/// Every connector tool name starts with this.
pub const PREFIX: &str = "connectors__";

/// The toolset label connector tools carry.
pub const TOOLSET: &str = "connectors";

/// A parsed connector tool name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectorName {
    /// The connector slug (`gmail`).
    pub connector: String,
    /// The tool part (`send_email`).
    pub tool: String,
}

impl ConnectorName {
    /// The vendor slugs to try, conventional (prefixed) first, then the
    /// literal tool part.
    #[must_use]
    pub fn slugs(&self) -> Vec<String> {
        let conventional = format!("{}_{}", self.connector, self.tool).to_uppercase();
        if conventional == self.tool {
            vec![conventional]
        } else {
            vec![conventional, self.tool.clone()]
        }
    }
}

/// Whether `name` is in the connector namespace.
#[must_use]
pub fn is_connector(name: &str) -> bool {
    name.starts_with(PREFIX)
}

/// Parses `connectors__<connector>__<tool>` (the tool part keeps its own
/// underscores).
#[must_use]
pub fn parse(name: &str) -> Option<ConnectorName> {
    let rest = name.strip_prefix(PREFIX)?;
    let (connector, tool) = rest.split_once("__")?;
    (!connector.is_empty() && !tool.is_empty()).then(|| ConnectorName {
        connector: connector.to_owned(),
        tool: tool.to_owned(),
    })
}

/// The model-facing name of vendor slug `tool` of `connector`, and whether
/// the slug was conventional (carried the connector prefix).
#[must_use]
pub fn compose(connector: &str, tool: &str) -> (String, bool) {
    let prefix = format!("{}_", connector.to_uppercase());
    let (short, conventional) = match tool.strip_prefix(&prefix) {
        Some(short) if !short.is_empty() => (short.to_lowercase(), true),
        _ => (tool.to_owned(), false),
    };
    (format!("{PREFIX}{connector}__{short}"), conventional)
}

/// Whether `alias` matches `^[a-z0-9][a-z0-9-]{0,31}$`.
#[must_use]
pub fn is_alias(alias: &str) -> bool {
    let mut chars = alias.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    alias.len() <= 32
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::{compose, is_alias, parse};

    #[test]
    fn conventional_slugs_lose_their_prefix_and_parse_back() {
        assert_eq!(
            compose("gmail", "GMAIL_SEND_EMAIL"),
            ("connectors__gmail__send_email".to_owned(), true)
        );
        assert_eq!(
            compose("gmail", "send_email"),
            ("connectors__gmail__send_email".to_owned(), false)
        );
        let name = parse("connectors__gmail__send_email").unwrap();
        assert_eq!(name.connector, "gmail");
        assert_eq!(name.tool, "send_email");
        assert_eq!(name.slugs(), ["GMAIL_SEND_EMAIL", "send_email"]);
        assert!(parse("connectors__gmail").is_none());
        assert!(parse("mcp__x__y").is_none());
    }

    #[test]
    fn aliases_follow_the_pattern() {
        assert!(is_alias("work"));
        assert!(is_alias("a1-b"));
        assert!(!is_alias("-work"));
        assert!(!is_alias("Work"));
        assert!(!is_alias(""));
        assert!(!is_alias(&"a".repeat(33)));
        assert!(is_alias(&"a".repeat(32)));
    }
}
