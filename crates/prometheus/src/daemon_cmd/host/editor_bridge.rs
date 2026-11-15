//! The daemon side of editor-native files and terminals (spec:
//! acp-adapter, Editor-native file access; Editor terminal delegation):
//! an ACP session's file reads and writes under its working directory and,
//! when allowed, its foreground terminal commands become server requests
//! sent only to the session's drivers whose client advertised the
//! matching feature (an ACP connection whose editor has the capability).
//! With no such driver attached the session uses its own tools.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pmt_config::schema::SandboxMode;
use pmt_gateway::{FeatureAnswer, Link};
use pmt_rpc::contracts::{
    EDITOR_FS, EDITOR_TERMINAL, EditorFsResult, EditorRead, EditorTerminal, EditorTerminalResult,
    EditorWrite, ServerRequest,
};
use pmt_tools::editor::{DelegateFuture, DelegateOutcome, Delegated, EditorFs, ForegroundDelegate};
use serde_json::json;

/// How long a file read or write through the editor may take.
const FS_WAIT: Duration = Duration::from_secs(30);

/// Extra time an editor terminal gets beyond the command's own timeout.
const TERMINAL_GRACE: Duration = Duration::from_secs(30);

/// A session's file reads and writes through its editor.
struct FsBridge {
    link: Link,
    runtime: tokio::runtime::Handle,
}

impl FsBridge {
    /// Asks the editor; `None` when no editor can answer for the session.
    fn ask(&self, method: &str, params: serde_json::Value) -> Option<io::Result<EditorFsResult>> {
        if !self.link.has_feature(EDITOR_FS) {
            return None;
        }
        let answer = self
            .runtime
            .block_on(self.link.ask_feature(method, params, FS_WAIT, EDITOR_FS));
        match answer {
            FeatureAnswer::NoClient => None,
            FeatureAnswer::Unanswered => Some(Err(io::Error::other(
                "the editor did not answer the file request",
            ))),
            FeatureAnswer::Answered(value) => Some(
                serde_json::from_value::<EditorFsResult>(value)
                    .map_err(|e| io::Error::other(format!("malformed editor answer: {e}"))),
            ),
        }
    }
}

impl EditorFs for FsBridge {
    fn read(&self, path: &Path) -> Option<io::Result<String>> {
        let params = json!({"session_id": self.link.session_id(), "path": path});
        let answer = self.ask(EditorRead::NAME, params)?;
        Some(
            answer.and_then(|result| match (result.error, result.content) {
                (Some(error), _) => Err(io::Error::other(error)),
                (None, content) => Ok(content.unwrap_or_default()),
            }),
        )
    }

    fn write(&self, path: &Path, text: &str) -> Option<io::Result<()>> {
        let params = json!({"session_id": self.link.session_id(), "path": path, "content": text});
        let answer = self.ask(EditorWrite::NAME, params)?;
        Some(answer.and_then(|result| match result.error {
            Some(error) => Err(io::Error::other(error)),
            None => Ok(()),
        }))
    }
}

/// A session's foreground commands in its editor's terminal.
struct TerminalBridge {
    link: Link,
}

impl ForegroundDelegate for TerminalBridge {
    fn run<'a>(&'a self, call: Delegated<'a>) -> DelegateFuture<'a> {
        Box::pin(async move {
            let params = json!({
                "session_id": self.link.session_id(),
                "command": call.command,
                "cwd": call.cwd,
                "output_byte_limit": call.output_bytes,
            });
            let wait = Duration::from_secs(call.timeout_secs) + TERMINAL_GRACE;
            let answer = self
                .link
                .ask_feature(EditorTerminal::NAME, params, wait, EDITOR_TERMINAL)
                .await;
            let result = match answer {
                FeatureAnswer::NoClient => return None,
                FeatureAnswer::Unanswered => EditorTerminalResult {
                    error: Some("the editor terminal did not answer".to_owned()),
                    ..EditorTerminalResult::default()
                },
                FeatureAnswer::Answered(value) => {
                    serde_json::from_value(value).unwrap_or_else(|e| EditorTerminalResult {
                        error: Some(format!("malformed editor terminal answer: {e}")),
                        ..EditorTerminalResult::default()
                    })
                }
            };
            Some(DelegateOutcome {
                output: result.output,
                truncated: result.truncated,
                exit_code: result.exit_code,
                error: result.error,
            })
        })
    }
}

/// Binds an ACP session's editor bridges: files always (they answer only
/// while a capable editor is attached), the terminal only when
/// `acp.use_editor_terminal` is on and the profile runs local commands
/// without the OS sandbox (`security.sandbox.mode = "off"`), since the
/// editor's terminal is outside it.
pub(super) fn bind(scope: &pmt_config::ProfileScope, session: &str, link: &Link) {
    let fs = FsBridge {
        link: link.clone(),
        runtime: tokio::runtime::Handle::current(),
    };
    pmt_tools::editor::bind_fs(session, Arc::new(fs));
    let config = scope.config();
    let unsandboxed = config.security.sandbox.mode == SandboxMode::Off;
    if config.acp.use_editor_terminal && unsandboxed {
        let delegate = TerminalBridge { link: link.clone() };
        pmt_tools::editor::bind_terminal(session, Arc::new(delegate));
    }
}
