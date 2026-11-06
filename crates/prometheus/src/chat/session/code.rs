//! A session's `execute_code` (spec: subagent-delegation, Programmatic
//! tool calling under the caller's grant; `execute_code` pre-execution
//! guards): the script runs on the managed Python runtime in the
//! session's own sandbox, and its stub calls go through the session's own
//! tool pipeline, so the grant, approvals and budgets are checked again
//! for every call. The approval policy and the self-termination guard
//! judge the script itself before this port runs (the dispatcher's gate).

use std::path::PathBuf;
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use pmt_agent::tools::CodePort;
use pmt_agent::turn::ToolPort as _;
use pmt_agent::turn::model::{BoxFuture, RequestedCall};
use pmt_config::ProfileScope;
use pmt_core::cancel::CancelToken;
use pmt_core::error::{Error, codes};
use pmt_sandbox::Sandbox;
use pmt_tools::ToolResult;
use pmt_tools::code_exec::{self, Execution, StubCalls};
use serde_json::Value;

use super::tools::SwappableTools;

/// The session's `execute_code`.
pub struct SessionCode {
    scope: ProfileScope,
    session: String,
    sandbox: Option<Arc<Sandbox>>,
    workdir: PathBuf,
    tools: OnceLock<Weak<SwappableTools>>,
}

impl SessionCode {
    /// `execute_code` of `session`, running in `sandbox` (none: refused).
    #[must_use]
    pub fn new(
        scope: &ProfileScope,
        session: &str,
        sandbox: Option<Arc<Sandbox>>,
        workdir: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self {
            scope: scope.clone(),
            session: session.to_owned(),
            sandbox,
            workdir,
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
        let Some(sandbox) = &self.sandbox else {
            return refused(code_exec::NO_SANDBOX);
        };
        let python = match crate::plugins_host::bridge::prepare::toolchain(&self.scope) {
            Ok((python, _)) => python.exe,
            Err(err) => return code_exec::refusal(&err),
        };
        let scratch = self.scope.path("cache/scratch").join(&self.session);
        let spill = pmt_tools::spillover::session_dir(self.scope.dir(), &self.session);
        let exec = Execution {
            code,
            python: &python,
            sandbox,
            workdir: &self.workdir,
            scratch: &scratch,
            tools: stubs,
            max_tool_calls: config.code_execution.max_tool_calls,
            timeout: Duration::from_secs(config.code_execution.timeout_secs),
            spill_dir: &spill,
        };
        code_exec::run(exec, &Stubs(tools), cancel).await
    }
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
