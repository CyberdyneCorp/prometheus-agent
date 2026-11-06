//! A session's `execute_code` (spec: subagent-delegation, Programmatic
//! tool calling under the caller's grant; `execute_code` pre-execution
//! guards): the script runs on the managed Python runtime in the
//! session's own sandbox, and its stub calls go through the session's own
//! tool pipeline, so the grant, approvals and budgets are checked again
//! for every call. The approval policy and the self-termination guard
//! judge the script itself before this port runs (the dispatcher's gate).
//!
//! On the local backend scripts run as cells of the session's persistent
//! kernel (spec: Persistent `execute_code` session kernel), on the
//! project's virtual environment or in a staging directory as
//! `code_execution.mode` says (spec: `execute_code` execution modes);
//! other backends run one-shot scripts and say so.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use pmt_agent::tools::CodePort;
use pmt_agent::turn::ToolPort as _;
use pmt_agent::turn::model::{BoxFuture, RequestedCall};
use pmt_config::ProfileScope;
use pmt_config::schema::CodeExecutionMode;
use pmt_core::cancel::CancelToken;
use pmt_core::error::{Error, codes};
use pmt_sandbox::Sandbox;
use pmt_tools::ToolResult;
use pmt_tools::code_exec::interpreter::{self, Choice, Project};
use pmt_tools::code_exec::{self, Execution, StubCalls, kernel};
use serde_json::Value;

use super::tools::SwappableTools;

/// Where a session's scripts run.
pub struct Placement {
    /// The session's sandbox (none: refused).
    pub sandbox: Option<Arc<Sandbox>>,
    /// Its working directory.
    pub workdir: PathBuf,
    /// `VIRTUAL_ENV` of its terminal environment.
    pub venv: Option<PathBuf>,
    /// A top-level session (not a delegated child).
    pub top_level: bool,
}

/// The session's `execute_code`.
pub struct SessionCode {
    scope: ProfileScope,
    session: String,
    place: Placement,
    tools: OnceLock<Weak<SwappableTools>>,
}

impl SessionCode {
    /// `execute_code` of `session`, running as `place` says.
    #[must_use]
    pub fn new(scope: &ProfileScope, session: &str, place: Placement) -> Arc<Self> {
        Arc::new(Self {
            scope: scope.clone(),
            session: session.to_owned(),
            place,
            tools: OnceLock::new(),
        })
    }

    /// The session's tools, once built (stub calls run through them).
    pub fn bind(&self, tools: &Arc<SwappableTools>) {
        let _ = self.tools.set(Arc::downgrade(tools));
    }

    async fn call(&self, arguments: &Value, cancel: &CancelToken) -> ToolResult {
        let code = arguments
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if code.trim().is_empty() {
            return ToolResult::error(code_exec::NO_CODE);
        }
        let Some(tools) = self.tools.get().and_then(Weak::upgrade) else {
            return ToolResult::error(code_exec::NO_SANDBOX);
        };
        let config = self.scope.config();
        let granted: Vec<String> = tools.definitions().into_iter().map(|t| t.name).collect();
        let stubs = match code_exec::stub_tools(&config.code_execution.allowed_tools, &granted) {
            Ok(stubs) => stubs,
            Err(err) => return code_exec::refusal(&err),
        };
        let Some(sandbox) = &self.place.sandbox else {
            return refused(code_exec::NO_SANDBOX);
        };
        let exec = &config.code_execution;
        let (python, note) = match self.python(exec.mode, sandbox).await {
            Ok(chosen) => chosen,
            Err(err) => return code_exec::refusal(&err),
        };
        let scratch = self.scope.path("cache/scratch").join(&self.session);
        let spill = pmt_tools::spillover::session_dir(self.scope.dir(), &self.session);
        let timeout = Duration::from_secs(exec.timeout_secs);
        let backend = &config.terminal.backend;
        if backend != "local" {
            let one_shot = format!(
                "The '{backend}' terminal backend keeps no Python kernel: the script ran as a \
                 one-shot script, so nothing persists between calls."
            );
            let run = Execution {
                code,
                python: &python,
                sandbox,
                workdir: &self.place.workdir,
                scratch: &scratch,
                tools: stubs,
                max_tool_calls: exec.max_tool_calls,
                timeout,
                spill_dir: &spill,
                note: Some(note.map_or(one_shot.clone(), |n| format!("{n} {one_shot}"))),
            };
            return code_exec::run(run, &Stubs(tools), cancel).await;
        }
        let key = kernel::Key {
            session: self.session.clone(),
            mode: exec.mode.as_str(),
            interpreter: python,
            workdir: self.place.workdir.clone(),
            tools: stubs,
        };
        let place = kernel::Placement {
            sandbox,
            scratch: &scratch,
            staging: exec.mode == CodeExecutionMode::Strict,
            top_level: self.place.top_level,
            idle: Duration::from_secs(exec.kernel_idle_timeout_secs),
            max_top_level: exec.max_session_kernels as usize,
        };
        let cell = kernel::Cell {
            code,
            max_tool_calls: exec.max_tool_calls,
            timeout,
            spill_dir: &spill,
            reset: arguments.get("reset").and_then(Value::as_bool) == Some(true),
        };
        let result = kernel::run(key, &place, &cell, &Stubs(tools), cancel).await;
        match note {
            Some(note) => with_note(result, &note),
            None => result,
        }
    }

    /// The interpreter for `mode` and the fallback note, if any.
    async fn python(
        &self,
        mode: CodeExecutionMode,
        sandbox: &Sandbox,
    ) -> Result<(PathBuf, Option<String>), Error> {
        let project = Project {
            active: self.place.venv.as_deref(),
            workdir: &self.place.workdir,
        };
        let note = match mode {
            CodeExecutionMode::Strict => None,
            CodeExecutionMode::Project => match interpreter::project(&project, sandbox).await {
                Choice::Use(python) => return Ok((python, None)),
                Choice::None => None,
                Choice::Fallback(note) => Some(note),
            },
        };
        let (managed, _) = crate::plugins_host::bridge::prepare::toolchain(&self.scope)?;
        Ok((managed.exe, note))
    }
}

/// `result` with `note` added to its JSON body.
fn with_note(mut result: ToolResult, note: &str) -> ToolResult {
    let Some(pmt_tools::result::Content::Text { text }) = result.content.first_mut() else {
        return result;
    };
    if let Ok(mut body) = serde_json::from_str::<Value>(text) {
        body["note"] = note.into();
        *text = body.to_string();
    }
    result
}

fn refused(message: &str) -> ToolResult {
    code_exec::refusal(&Error::new(&codes::EXECUTE_CODE_REFUSED, message))
}

impl CodePort for SessionCode {
    fn call<'a>(&'a self, arguments: Value, cancel: &'a CancelToken) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move { SessionCode::call(self, &arguments, cancel).await })
    }
}

/// Stub calls through the session's tool pipeline.
struct Stubs(Arc<SwappableTools>);

impl StubCalls for Stubs {
    fn call<'a>(&'a self, tool: &'a str, arguments: Value) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let call = RequestedCall {
                id: format!("code_{}", ulid::Ulid::generate()),
                name: tool.to_owned(),
                arguments: arguments.to_string(),
            };
            let cancel = CancelToken::new();
            let batch = self
                .0
                .run(std::slice::from_ref(&call), "execute_code", &cancel)
                .await;
            batch.outcomes.into_iter().next().map_or_else(
                || ToolResult::error("the tool call did not run"),
                |o| o.result,
            )
        })
    }
}
