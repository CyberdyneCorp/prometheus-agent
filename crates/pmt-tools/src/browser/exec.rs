//! `browser_exec` (spec: execution-environments, `browser_exec` tool).
//!
//! The model's Python runs on the managed CPython runtime under the
//! session's OS sandbox, in a fresh interpreter per call, with the page
//! helpers pre-imported. Each helper call travels as one JSON line on the
//! interpreter's stdout and is answered on its stdin by this process,
//! which drives the browser over CDP; so the browser (tabs, cookies, the
//! current page) persists across calls while interpreter variables do
//! not. Each session key has a workspace directory that persists too.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use pmt_sandbox::Sandbox;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{ChildStdin, Command};

use super::launch::RuntimePolicy;
use super::session::{BrowserManager, BrowserSession};
use crate::result::{Content, Status, ToolResult};
use crate::terminal::env::ChildEnv;

/// The helper module every call starts with.
const PRELUDE: &str = include_str!("prelude.py");
/// `timeout_secs` default.
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// `timeout_secs` bounds.
pub const TIMEOUT_RANGE: (u64, u64) = (1, 1_800);

/// Everything `browser_exec` needs in one session.
pub struct ExecSetup {
    /// The profile's browser sessions.
    pub manager: Arc<BrowserManager>,
    /// Where the CPython executable comes from (runtime `cpython`).
    pub python: RuntimePolicy,
    /// The session's OS sandbox.
    pub sandbox: Arc<Sandbox>,
    /// The interpreter's environment.
    pub env: ChildEnv,
    /// Workspaces live here, one directory per `session` name (inside
    /// the session scratch directory, which the sandbox lets it write).
    pub workspace_root: PathBuf,
    /// The active model accepts image input (screenshots are attached).
    pub image_input: bool,
}

/// One call's arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecArgs {
    /// Python source.
    pub code: String,
    /// Named browser session (own driver and browser); `default` if unset.
    pub session: Option<String>,
    /// Bound on the call.
    pub timeout_secs: Option<u64>,
}

/// `browser_exec` of one session.
pub struct BrowserExec {
    setup: ExecSetup,
}

/// What one run produced.
struct Run {
    output: String,
    error: Option<String>,
    screenshots: Vec<PathBuf>,
    timed_out: bool,
}

impl BrowserExec {
    /// The tool over `setup`.
    #[must_use]
    pub fn new(setup: ExecSetup) -> Self {
        Self { setup }
    }

    /// Runs one call of session `session_id`.
    pub async fn exec(&self, session_id: &str, args: &ExecArgs) -> ToolResult {
        let name = args.session.as_deref().unwrap_or("default");
        if !is_plain_name(name) {
            return ToolResult::error("session must be a plain name (letters, digits, - and _)");
        }
        let browser = match self
            .setup
            .manager
            .session(&format!("{session_id}/{name}"))
            .await
        {
            Ok(browser) => browser,
            Err(why) => return ToolResult::error(format!("browser_exec: {why}")),
        };
        let workspace = self.setup.workspace_root.join(name);
        if let Err(err) = pmt_core::private_fs::create_dir(&workspace) {
            return ToolResult::error(format!("browser_exec: cannot create the workspace: {err}"));
        }
        let limit = Duration::from_secs(
            args.timeout_secs
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .clamp(TIMEOUT_RANGE.0, TIMEOUT_RANGE.1),
        );
        let run = match self.run(&browser, &workspace, &args.code, limit).await {
            Ok(run) => run,
            Err(why) => return ToolResult::error(format!("browser_exec: {why}")),
        };
        self.result(&browser, name, &workspace, &run, limit)
    }

    async fn run(
        &self,
        browser: &BrowserSession,
        workspace: &Path,
        code: &str,
        limit: Duration,
    ) -> Result<Run, String> {
        let python = self.setup.python.executable("cpython").await?;
        let mut std_command = self
            .setup
            .sandbox
            .command(&python)
            .map_err(|err| err.message().to_owned())?;
        self.setup.env.apply(&mut std_command);
        std_command
            .args(["-I", "-u", "-c", PRELUDE])
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut command = Command::from(std_command);
        command.kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|err| format!("cannot start {}: {err}", python.display()))?;
        let (Some(mut stdin), Some(stdout), Some(mut stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err("the interpreter's pipes are missing".to_owned());
        };
        let errors = tokio::spawn(async move {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text).await;
            text
        });
        send(&mut stdin, &json!({"code": code})).await?;
        let mut session = Exchange {
            browser,
            workspace,
            stdin,
            screenshots: Vec::new(),
        };
        let outcome = tokio::time::timeout(limit, session.serve(BufReader::new(stdout))).await;
        let _ = child.start_kill();
        let stderr_text = errors.await.unwrap_or_default();
        let screenshots = std::mem::take(&mut session.screenshots);
        Ok(match outcome {
            Err(_) => Run {
                output: String::new(),
                error: None,
                screenshots,
                timed_out: true,
            },
            Ok(Some((output, error))) => Run {
                output,
                error,
                screenshots,
                timed_out: false,
            },
            Ok(None) => Run {
                output: String::new(),
                error: Some(format!(
                    "the interpreter exited early: {}",
                    stderr_text.trim()
                )),
                screenshots,
                timed_out: false,
            },
        })
    }

    fn result(
        &self,
        browser: &BrowserSession,
        name: &str,
        workspace: &Path,
        run: &Run,
        limit: Duration,
    ) -> ToolResult {
        let shots: Vec<String> = run
            .screenshots
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        let mut body = json!({
            "output": run.output,
            "error": run.error,
            "session": name,
            "workspace": workspace.display().to_string(),
            "screenshots": shots,
        });
        let handled = browser.dialogs().take_handled();
        if !handled.is_empty() {
            body["dialogs"] = json!(handled);
        }
        if browser.fallback_from_cloud() {
            body["fallback_from_cloud"] = json!(true);
        }
        let status = if run.timed_out {
            body["error"] = json!(format!(
                "browser_exec timed out after {} s",
                limit.as_secs()
            ));
            Status::Timeout
        } else if run.error.is_some() {
            Status::Error
        } else {
            Status::Ok
        };
        let mut content = vec![Content::Text {
            text: body.to_string(),
        }];
        if self.setup.image_input {
            content.extend(shots.into_iter().map(|path| Content::Media {
                mime: "image/png".to_owned(),
                path,
            }));
        }
        ToolResult {
            status,
            content,
            metadata: serde_json::Map::new(),
        }
    }
}

/// The interpreter's side of one call.
struct Exchange<'a> {
    browser: &'a BrowserSession,
    workspace: &'a Path,
    stdin: ChildStdin,
    screenshots: Vec<PathBuf>,
}

impl Exchange<'_> {
    /// Answers helper calls until the code finishes: `Some((output,
    /// error))`, or `None` when the interpreter exits without finishing.
    async fn serve<R: tokio::io::AsyncBufRead + Unpin>(
        &mut self,
        reader: R,
    ) -> Option<(String, Option<String>)> {
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message.get("done").is_some() {
                let output = message["output"].as_str().unwrap_or_default().to_owned();
                let error = message["error"].as_str().map(str::to_owned);
                return Some((output, error));
            }
            let name = message["call"].as_str().unwrap_or_default();
            let args = message["args"].as_array().cloned().unwrap_or_default();
            let reply = match self.helper(name, &args).await {
                Ok(value) => json!({"ok": value}),
                Err(why) => json!({"error": why}),
            };
            send(&mut self.stdin, &reply).await.ok()?;
        }
        None
    }

    async fn helper(&mut self, name: &str, args: &[Value]) -> Result<Value, String> {
        let text = |at: usize| args.get(at).and_then(Value::as_str);
        let needed = |at: usize| {
            text(at).ok_or_else(|| format!("{name}: argument {} must be a string", at + 1))
        };
        let number = |at: usize| args.get(at).and_then(Value::as_f64);
        let b = self.browser;
        match name {
            "new_tab" => b.new_tab(text(0)).await,
            "goto_url" => b.goto_url(needed(0)?).await,
            "wait_for_load" => {
                let secs = number(0).unwrap_or(30.0).clamp(0.1, 300.0);
                b.wait_for_load(Duration::from_secs_f64(secs)).await
            }
            "page_info" => b.page_info().await,
            "js" => b.evaluate(needed(0)?).await,
            "fill_input" => b.fill_input(needed(0)?, needed(1)?).await,
            "click_at_xy" => {
                b.click_at_xy(number(0).unwrap_or_default(), number(1).unwrap_or_default())
                    .await
            }
            "capture_screenshot" => {
                let path = b.capture_screenshot(self.workspace, text(0)).await?;
                self.screenshots.push(path.clone());
                Ok(json!(path.display().to_string()))
            }
            "cdp" => {
                b.raw_cdp(
                    needed(0)?,
                    args.get(1).cloned().unwrap_or_else(|| json!({})),
                )
                .await
            }
            "ensure_real_tab" => b.ensure_real_tab().await,
            other => Err(format!("unknown helper {other}")),
        }
    }
}

async fn send(stdin: &mut ChildStdin, message: &Value) -> Result<(), String> {
    let mut line = message.to_string();
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|err| format!("cannot reach the interpreter: {err}"))?;
    stdin
        .flush()
        .await
        .map_err(|err| format!("cannot reach the interpreter: {err}"))
}

fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}
