//! Where a tool result came from (spec: tool-system, Tool result
//! provenance tagging): the dispatcher tags every result with
//! `{source, trust}` after the result budget. A tool that declares its own
//! provenance (a connector) keeps it; otherwise the owner and the tool
//! decide: MCP servers and plugins are untrusted, and so are the built-in
//! tools that bring outside text in (web, browser, command output, vision)
//! and file reads outside the session's workspace.

use std::path::Path;

use pmt_core::provenance::Provenance;
use serde_json::Value;

use crate::descriptor::{Owner, RegisteredTool};
use crate::files::guards::{absolute, resolve_existing};

/// Built-in tools whose results carry outside content, by source.
const OUTSIDE: &[(&str, &[&str])] = &[
    ("web", &["web_search", "web_extract", "x_search"]),
    (
        "browser",
        &[
            "browser_exec",
            "browser_cdp",
            "browser_dialog",
            "browser_vault",
        ],
    ),
    ("terminal", &["terminal", "process", "execute_code"]),
    ("vision", &["vision_analyze", "video_analyze"]),
    ("messaging", &["send_message"]),
];

/// File tools whose trust depends on the path read.
const FILE_READS: &[&str] = &["read_file", "search_files"];

/// The provenance of a result of `tool` called with `arguments` in a
/// session whose workspace is `workspace` (`None`: unknown, so file reads
/// are untrusted).
#[must_use]
pub fn classify(tool: &RegisteredTool, arguments: &Value, workspace: Option<&Path>) -> Provenance {
    if let Some(declared) = tool.provenance.as_ref().and_then(Provenance::from_value) {
        return declared;
    }
    let name = tool.name();
    match &tool.owner {
        Owner::Mcp(server) => Provenance::untrusted(format!("mcp:{server}")),
        Owner::Plugin(id) => Provenance::untrusted(format!("plugin:{id}")),
        Owner::Capability(owner) => Provenance::trusted(owner.clone()),
        Owner::Builtin if FILE_READS.contains(&name) => file(arguments, workspace),
        Owner::Builtin => OUTSIDE
            .iter()
            .find(|(_, tools)| tools.contains(&name))
            .map_or_else(
                || Provenance::trusted("prometheus"),
                |(source, _)| Provenance::untrusted(*source),
            ),
    }
}

/// A file read: trusted inside the workspace, untrusted elsewhere.
fn file(arguments: &Value, workspace: Option<&Path>) -> Provenance {
    let Some(workspace) = workspace else {
        return Provenance::untrusted("file");
    };
    let raw = arguments.get("path").and_then(Value::as_str).unwrap_or(".");
    let path = resolve_existing(&absolute(raw, workspace, None));
    if path.starts_with(resolve_existing(workspace)) {
        Provenance::trusted("file")
    } else {
        Provenance::untrusted("file")
    }
}

#[cfg(test)]
mod tests {
    use pmt_core::provenance::Trust;
    use serde_json::json;

    use super::*;

    fn tool(name: &str, owner: Owner) -> RegisteredTool {
        crate::testing::tool(name, "test", owner)
    }

    #[test]
    fn outside_sources_are_untrusted_and_workspace_files_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Some(dir.path());
        let mcp = classify(
            &tool("mcp__gh__issues", Owner::Mcp("gh".into())),
            &json!({}),
            ws,
        );
        assert_eq!(mcp, Provenance::untrusted("mcp:gh"));
        let web = classify(&tool("web_extract", Owner::Builtin), &json!({}), ws);
        assert_eq!(web.trust, Trust::Untrusted);
        let inside = classify(
            &tool("read_file", Owner::Builtin),
            &json!({"path": "notes.txt"}),
            ws,
        );
        assert_eq!(inside, Provenance::trusted("file"));
        let outside = classify(
            &tool("read_file", Owner::Builtin),
            &json!({"path": "/etc/hosts"}),
            ws,
        );
        assert_eq!(outside, Provenance::untrusted("file"));
        let escape = classify(
            &tool("read_file", Owner::Builtin),
            &json!({"path": "../x"}),
            ws,
        );
        assert_eq!(escape.trust, Trust::Untrusted);
        let todo = classify(&tool("todo", Owner::Builtin), &json!({}), ws);
        assert_eq!(todo, Provenance::trusted("prometheus"));
    }
}
