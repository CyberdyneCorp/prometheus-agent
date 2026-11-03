//! MCP tool names and filters (spec: tool-system, MCP tool naming and
//! filtering).
//!
//! A server's tool `t` registers as `mcp__<server>__<t>` with every
//! character outside `[A-Za-z0-9_]` replaced by `_`; a name longer than 64
//! characters keeps its first 55 and ends with `_` and the first 8 hex
//! characters of the SHA-256 of the whole name, so long names stay unique.
//! The tools live in toolset `mcp-<server>`.

use pmt_config::schema::McpToolFilter;
use std::fmt::Write as _;

use sha2::{Digest as _, Sha256};

/// The longest registered name.
pub const MAX_NAME: usize = 64;
/// Characters kept from a name that is too long.
const KEPT: usize = 55;

/// The toolset of `server`'s tools.
#[must_use]
pub fn toolset(server: &str) -> String {
    format!("mcp-{server}")
}

/// The registered name of `server`'s tool `tool`.
#[must_use]
pub fn tool_name(server: &str, tool: &str) -> String {
    let full = format!("mcp__{}__{}", sanitize(server), sanitize(tool));
    if full.len() <= MAX_NAME {
        return full;
    }
    let hash = Sha256::digest(full.as_bytes());
    let hex = hash.iter().take(4).fold(String::new(), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    });
    format!("{}_{hex}", &full[..KEPT])
}

fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Which of a server's tools register: `include` is a whitelist that wins
/// over `exclude` (an empty `include` registers nothing); entries are
/// exact names or case-sensitive globs over the server's own tool names.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    include: Option<Vec<glob::Pattern>>,
    exclude: Vec<glob::Pattern>,
}

impl Filter {
    /// The filter of `[mcp.servers.<name>.tools]` (patterns that do not
    /// parse were refused when the configuration loaded; they match
    /// nothing here).
    #[must_use]
    pub fn of(config: &McpToolFilter) -> Self {
        let patterns = |list: &[String]| -> Vec<glob::Pattern> {
            list.iter()
                .filter_map(|p| glob::Pattern::new(p).ok())
                .collect()
        };
        Self {
            include: config.include.as_deref().map(patterns),
            exclude: patterns(&config.exclude),
        }
    }

    /// Whether the server's tool `name` registers.
    #[must_use]
    pub fn admits(&self, name: &str) -> bool {
        match &self.include {
            Some(include) => include.iter().any(|p| p.matches(name)),
            None => !self.exclude.iter().any(|p| p.matches(name)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Filter, tool_name};
    use pmt_config::schema::McpToolFilter;

    fn filter(include: Option<&[&str]>, exclude: &[&str]) -> Filter {
        let owned = |l: &[&str]| l.iter().map(|&s| s.to_owned()).collect::<Vec<_>>();
        Filter::of(&McpToolFilter {
            include: include.map(owned),
            exclude: owned(exclude),
        })
    }

    #[test]
    fn names_are_sanitized_and_long_ones_hashed_to_64_characters() {
        assert_eq!(
            tool_name("git-hub", "do.thing-x"),
            "mcp__git_hub__do_thing_x"
        );
        assert_eq!(tool_name("gh", "createIssue"), "mcp__gh__createIssue");
        let long = tool_name("server", &"a".repeat(80));
        assert_eq!(long.len(), 64);
        assert!(long.starts_with(&format!("mcp__server__{}", "a".repeat(42))));
        assert_eq!(&long[55..56], "_");
        assert!(long[56..].chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(long, tool_name("server", &format!("{}b", "a".repeat(80))));
    }

    #[test]
    fn include_beats_exclude() {
        let f = filter(Some(&["create_issue"]), &["create_issue", "delete_issue"]);
        assert!(f.admits("create_issue"));
        assert!(!f.admits("delete_issue"));
        assert!(!f.admits("list_issues"));
    }

    #[test]
    fn an_empty_include_admits_nothing_and_globs_are_case_sensitive() {
        assert!(!filter(Some(&[]), &[]).admits("anything"));
        let f = filter(None, &["delete_*"]);
        assert!(!f.admits("delete_repo"));
        assert!(f.admits("Delete_repo"));
        assert!(f.admits("create_issue"));
    }
}
