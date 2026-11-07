//! The per-execution RPC channel between a script's stubs and the host
//! (spec: subagent-delegation, `execute_code` RPC channel and limits).
//!
//! The channel is a private directory of the execution: a stub writes a
//! request file and waits for its answer file. It needs no network, so it
//! works under a sandbox that forbids every connection, and it is
//! authenticated by a 256-bit random token compared in constant time: a
//! request with an empty or wrong token gets `Unauthorized RPC request`.
//! The host answers only the execution's stub tools, counts each call it
//! dispatches against `code_execution.max_tool_calls`, and strips the
//! background, PTY and notification options from `terminal` calls.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use crate::ToolResult;

/// The variable naming the channel directory.
pub const DIR_VAR: &str = "PMT_RPC_DIR";
/// The variable holding the token.
pub const TOKEN_VAR: &str = "PMT_RPC_TOKEN";
/// The answer to an unauthenticated request.
#[must_use]
pub fn unauthorized() -> &'static str {
    "Unauthorized RPC request"
}

/// `terminal` options a script may not use.
const TERMINAL_STRIPPED: &[&str] = &["background", "pty", "notify_on_complete", "watch_patterns"];

/// How often the host looks for requests.
const POLL: Duration = Duration::from_millis(10);

/// Runs a stub's call through the session's tool pipeline (which checks
/// the session's grant, approvals and budgets again).
pub trait StubCalls: Send + Sync {
    /// Calls `tool` with `arguments`.
    fn call<'a>(
        &'a self,
        tool: &'a str,
        arguments: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + 'a>>;
}

/// A 256-bit random token, lower-case hex.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    /// A fresh token.
    #[must_use]
    pub fn new() -> Self {
        let mut bytes = [0u8; 32];
        if getrandom::fill(&mut bytes).is_err() {
            // No OS randomness: refuse every request rather than accept a
            // guessable token.
            return Self(String::new());
        }
        Self(bytes.iter().fold(String::with_capacity(64), |mut out, b| {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
            out
        }))
    }

    /// The token text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `offered` is this token (constant time; an empty token
    /// never matches).
    #[must_use]
    pub fn accepts(&self, offered: &str) -> bool {
        let (a, b) = (self.0.as_bytes(), offered.as_bytes());
        !a.is_empty()
            && a.len() == b.len()
            && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }
}

impl Default for Token {
    fn default() -> Self {
        Self::new()
    }
}

/// The host side of one execution's channel.
pub struct Server {
    dir: PathBuf,
    token: Token,
    tools: Vec<String>,
    max_calls: u32,
    calls: AtomicU32,
}

impl Server {
    /// The channel in `dir`.
    #[must_use]
    pub fn new(dir: PathBuf, token: Token, tools: Vec<String>, max_calls: u32) -> Self {
        Self {
            dir,
            token,
            tools,
            max_calls,
            calls: AtomicU32::new(0),
        }
    }

    /// Stub calls dispatched so far.
    #[must_use]
    pub fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }

    /// Answers requests until dropped.
    pub async fn serve(&self, calls: &dyn StubCalls) {
        loop {
            for request in self.pending() {
                self.answer(&request, calls).await;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    fn pending(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut found: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "req"))
            .collect();
        found.sort();
        found
    }

    async fn answer(&self, request: &std::path::Path, calls: &dyn StubCalls) {
        let text = std::fs::read_to_string(request).unwrap_or_default();
        let _ = std::fs::remove_file(request);
        let reply = self.reply(&text, calls).await;
        let answer = request.with_extension("resp");
        let temp = request.with_extension("resp.tmp");
        if std::fs::write(&temp, reply.to_string()).is_ok() {
            let _ = std::fs::rename(&temp, &answer);
        }
    }

    /// The answer to one request text.
    pub async fn reply(&self, text: &str, calls: &dyn StubCalls) -> Value {
        let request: Value = serde_json::from_str(text).unwrap_or(Value::Null);
        let offered = request
            .get("token")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !self.token.accepts(offered) {
            return json!({"ok": false, "result": unauthorized()});
        }
        let tool = request
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !self.tools.iter().any(|t| t == tool) {
            return json!({"ok": false, "result": format!(
                "Tool '{tool}' is not available in execute_code. Available: {}",
                self.tools.join(", ")
            )});
        }
        let taken = self.calls.fetch_add(1, Ordering::SeqCst);
        if taken >= self.max_calls {
            return json!({"ok": false, "result": format!("Tool call limit reached ({}).", self.max_calls)});
        }
        let mut arguments = request.get("args").cloned().unwrap_or_else(|| json!({}));
        if tool == "terminal"
            && let Some(object) = arguments.as_object_mut()
        {
            for key in TERMINAL_STRIPPED {
                object.remove(*key);
            }
        }
        super::stub_reply(&calls.call(tool, arguments).await)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Recording(Mutex<Vec<(String, Value)>>);

    impl StubCalls for Recording {
        fn call<'a>(
            &'a self,
            tool: &'a str,
            arguments: Value,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + 'a>> {
            self.0.lock().unwrap().push((tool.to_owned(), arguments));
            Box::pin(async { ToolResult::ok("done") })
        }
    }

    fn server(tools: &[&str], max: u32) -> (Server, Token) {
        let token = Token::new();
        let tools = tools.iter().map(|&t| t.to_owned()).collect();
        (
            Server::new(PathBuf::from("."), token.clone(), tools, max),
            token,
        )
    }

    #[tokio::test]
    async fn wrong_or_empty_tokens_are_unauthorized() {
        let (server, _) = server(&["read_file"], 50);
        let calls = Recording::default();
        for token in ["", "nope"] {
            let reply = server
                .reply(
                    &json!({"token": token, "tool": "read_file"}).to_string(),
                    &calls,
                )
                .await;
            assert_eq!(reply["result"], "Unauthorized RPC request");
        }
        assert!(calls.0.lock().unwrap().is_empty());
    }

    /// Scenario: Stub limited to the grant.
    #[tokio::test]
    async fn a_tool_outside_the_stubs_is_refused_without_using_the_budget() {
        let (server, token) = server(&["read_file", "search_files"], 50);
        let calls = Recording::default();
        let reply = server
            .reply(
                &json!({"token": token.as_str(), "tool": "terminal", "args": {}}).to_string(),
                &calls,
            )
            .await;
        assert_eq!(
            reply["result"],
            "Tool 'terminal' is not available in execute_code. Available: read_file, search_files"
        );
        assert_eq!(server.calls(), 0);
    }

    /// Scenario: Call budget enforced.
    #[tokio::test]
    async fn the_fifty_first_call_is_refused_and_later_ones_too() {
        let (server, token) = server(&["read_file"], 50);
        let calls = Recording::default();
        let request = json!({"token": token.as_str(), "tool": "read_file", "args": {"path": "x"}})
            .to_string();
        for _ in 0..50 {
            assert_eq!(server.reply(&request, &calls).await["ok"], true);
        }
        for _ in 0..2 {
            assert_eq!(
                server.reply(&request, &calls).await["result"],
                "Tool call limit reached (50)."
            );
        }
        assert_eq!(calls.0.lock().unwrap().len(), 50);
    }

    #[tokio::test]
    async fn terminal_calls_lose_background_pty_and_notifications() {
        let (server, token) = server(&["terminal"], 50);
        let calls = Recording::default();
        let request = json!({"token": token.as_str(), "tool": "terminal",
            "args": {"command": "ls", "background": true, "pty": true, "notify_on_complete": true}});
        server.reply(&request.to_string(), &calls).await;
        assert_eq!(calls.0.lock().unwrap()[0].1, json!({"command": "ls"}));
    }
}
