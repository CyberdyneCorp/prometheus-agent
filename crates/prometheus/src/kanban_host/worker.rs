//! `prometheus kanban work --run <run-id>` (spec: kanban, Worker spawn by
//! run ticket; Worker liveness and heartbeats; Workspaces; Per-task cost
//! budget; Eval and harness-evolution tasks). The worker reads its run
//! ticket from stdin, consumes it, opens the assignee profile with that
//! profile's own secrets only (never the launch environment's), prepares
//! the task's workspace, heartbeats while it works and runs one agent
//! session whose `kanban_*` tools are scoped to its board and run. A run
//! still open when the session ends is left to the dispatcher with the
//! exit code recorded: 75 (rate limited), 78 (credential, model or
//! capability gone) or 0/1.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use pmt_agent::turn::events::EventSink;
use pmt_agent::turn::{ExitReason, TurnInput, TurnOptions};
use pmt_config::{Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::cancel::CancelToken;
use pmt_core::error::{Error, ErrorKind, codes};
use pmt_kanban::board::runs::{EXIT_RATE_LIMITED, EXIT_TERMINAL};
use pmt_kanban::dispatch::HEARTBEAT_MS;
use pmt_kanban::ticket::Ticket;
use pmt_kanban::{Actor, BlockKind, Board, Boards, Role, Run, Task, TaskKind, Workspace};
use pmt_store::{EndReason, Store, UsageScope};

use super::tool::Grant;
use crate::chat::session::kanban::KanbanRun;
use crate::chat::session::model::RouteFlags;
use crate::chat::session::{Clients, Factory, Options};
use crate::context::Context;

/// The longest ticket line read from stdin.
const TICKET_BYTES: u64 = 4096;

/// How a worker's session ended.
struct Ending {
    error: Option<Error>,
    session: Option<String>,
}

/// Runs the worker of `run`; returns its exit code.
///
/// # Errors
///
/// `kanban.ticket_refused` (exit 78) for a missing, reused or foreign
/// ticket; home and store errors.
pub fn work(ctx: &Context<'_>, run: &str) -> Result<u8, Error> {
    let mut input = String::new();
    let _ = std::io::stdin()
        .take(TICKET_BYTES)
        .read_to_string(&mut input);
    let ticket = Ticket::parse(&input, run)?;
    let home = ctx.home()?;
    let boards = Boards::open(home.root())?;
    let mut board = boards.board(&ticket.board)?;
    let row = board.use_ticket(run, &ticket.secret)?;
    let task = board.task(&row.task_id)?;
    let name = ProfileName::parse(&row.profile)?;
    // The assignee's own secrets store only: a worker never sees the
    // dispatcher's (or the launch environment's) credentials.
    let scope = ProfileScope::open(
        home.existing_profile(&name)?,
        &Overrides::default(),
        Launch::Other,
    )?;
    crate::startup::bind_profile(&scope);
    let workspace = match prepare(&board, &task, &scope) {
        Ok(path) => path,
        Err(reason) => {
            board.block(
                &worker_actor(&row),
                &task.id,
                BlockKind::Capability,
                &reason,
                pmt_kanban::now_ms(),
            )?;
            return Ok(0);
        }
    };
    board.set_workspace_path(&task.id, &workspace)?;
    let beating = Heartbeat::start(
        boards.clone(),
        &ticket.board,
        &row,
        super::ttl_ms(&scope.config().kanban),
    );
    let ending = match &task.kind {
        TaskKind::Agent => {
            crate::model_cmd::block_on(session(ctx, &scope, &board, &row, &task, &workspace))?
        }
        TaskKind::Eval { suite, .. } => eval(&mut board, &row, &task, suite, &workspace),
        TaskKind::Evolve { .. } => Ending {
            error: Some(Error::new(
                &codes::KANBAN_NOT_PERMITTED,
                "harness evolution is not available in this build: evolve tasks need `prometheus evolve`",
            )),
            session: None,
        },
    };
    beating.stop();
    finish(&mut board, &row, &task, &scope, &ending)
}

fn worker_actor(row: &Run) -> Actor {
    if row.role == Role::Reviewer.as_str() {
        Actor::reviewer(&row.profile)
    } else {
        Actor::worker(&row.profile, &row.id)
    }
}

/// The task's workspace directory; `Err` names why it cannot exist
/// (the task is blocked with kind `capability`).
fn prepare(board: &Board, task: &Task, scope: &ProfileScope) -> Result<PathBuf, String> {
    match &task.workspace {
        Workspace::Scratch => {
            let dir = board.scratch_dir(&task.id);
            pmt_core::private_fs::create_dir(&dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            Ok(dir)
        }
        Workspace::Dir { path } if path.is_dir() => Ok(path.clone()),
        Workspace::Dir { path } => Err(format!("workspace dir {} does not exist", path.display())),
        Workspace::Worktree { branch } => worktree(task, branch.as_deref(), scope),
    }
}

/// A git worktree of the repository at the profile's `terminal.cwd`, on
/// `kanban/<task>` or the given `<prefix>/<name>` branch; an existing
/// tree (a retried run) is reused.
fn worktree(task: &Task, branch: Option<&str>, scope: &ProfileScope) -> Result<PathBuf, String> {
    let repo = scope
        .config()
        .terminal
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .ok_or_else(|| {
            format!(
                "a worktree workspace needs a git repository: set terminal.cwd of profile '{}'",
                scope.name()
            )
        })?;
    let (prefix, name) = match branch.and_then(|b| b.rsplit_once('/')) {
        Some((prefix, name)) => (prefix.to_owned(), name.to_owned()),
        None => ("kanban".to_owned(), branch.unwrap_or(&task.id).to_owned()),
    };
    let request = pmt_worktrees::create::Request {
        name: Some(&name),
        branch_prefix: &prefix,
        sync_remote: false,
        ..pmt_worktrees::create::Request::default()
    };
    match pmt_worktrees::create::create(&repo, &request) {
        Ok(created) => Ok(created.tree.path),
        Err(err) => {
            let existing = pmt_worktrees::repo::Repo::discover(&repo)
                .map(|r| r.trees_dir().join(&name))
                .ok()
                .filter(|p| p.is_dir());
            existing.ok_or_else(|| err.message().to_owned())
        }
    }
}

/// Renews the claim lease every minute while the worker runs.
struct Heartbeat {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Heartbeat {
    fn start(boards: Boards, slug: &str, row: &Run, ttl_ms: i64) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let (slug, run, actor) = (slug.to_owned(), row.id.clone(), worker_actor(row));
        let thread = std::thread::spawn(move || {
            let mut since = 0_i64;
            while !flag.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(1));
                since += 1000;
                if since >= HEARTBEAT_MS {
                    since = 0;
                    if let Ok(mut board) = boards.board(&slug) {
                        let _ = board.heartbeat(&actor, &run, None, ttl_ms, pmt_kanban::now_ms());
                    }
                }
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The profile as a worker of `task` sees it: no background memory
/// review, and the task's remaining budget as the root lineage's limit
/// (delegated children included).
fn worker_scope(scope: &ProfileScope, task: &Task, spent: (f64, u64)) -> ProfileScope {
    let usd = task.budget_usd.map(|b| (b - spent.0).max(0.0));
    let tokens = task.budget_tokens.map(|b| b.saturating_sub(spent.1).max(1));
    scope.patched(move |config| {
        config.memory.nudge_interval = 0;
        if let Some(usd) = usd {
            let current = config.budgets.root_usd;
            config.budgets.root_usd = if current > 0.0 { current.min(usd) } else { usd };
        }
        if let Some(tokens) = tokens {
            let current = config.budgets.root_tokens;
            config.budgets.root_tokens = if current > 0 {
                current.min(tokens)
            } else {
                tokens
            };
        }
    })
}

/// The follow-up when a turn ends without a terminator.
const NUDGE: &str = "You ended your turn without finishing the task. Finish it now with exactly one of \
    kanban_complete (with evidence), kanban_request_review or kanban_block.";

async fn session(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    board: &Board,
    row: &Run,
    task: &Task,
    workspace: &Path,
) -> Ending {
    let mut ending = Ending {
        error: None,
        session: None,
    };
    if let Err(err) = Box::pin(agent(ctx, scope, board, row, task, workspace, &mut ending)).await {
        ending.error = Some(err);
    }
    ending
}

async fn agent(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    board: &Board,
    row: &Run,
    task: &Task,
    workspace: &Path,
    ending: &mut Ending,
) -> Result<(), Error> {
    let scope = worker_scope(scope, task, board.spend(&task.id)?);
    let store = Arc::new(Store::open(&scope)?);
    let run = KanbanRun {
        grant: Grant {
            board: board.slug().to_owned(),
            task: task.id.clone(),
            run: row.id.clone(),
            profile: row.profile.clone(),
            role: if row.role == Role::Reviewer.as_str() {
                Role::Reviewer
            } else {
                Role::Worker
            },
        },
        blobs: board.blobs().to_path_buf(),
    };
    let options = Options {
        interactive: false,
        cwd: workspace.to_path_buf(),
        source_tag: Some(run.source_tag()),
        kanban: Some(run),
        ..Options::default()
    };
    let flags = RouteFlags {
        provider: None,
        model: None,
        mock_script: None,
    };
    let factory = Factory::with_store(
        scope.clone(),
        Arc::clone(&store),
        ctx.launch,
        &flags,
        None,
        options,
    )?;
    let target = factory.models.primary(&flags).await?;
    let id = factory.create_session(None, &target)?;
    ending.session = Some(id.to_string());
    let clients = Clients {
        events: Arc::new(pmt_agent::turn::events::NoEvents) as Arc<dyn EventSink>,
        secrets: Arc::new(NoPrompt),
        completions: None,
        sudo: None,
        delegation: None,
    };
    let live = factory.open_with(id, &target, Some(clients)).await?;
    let mut prompt = pmt_kanban::context::render(board, &task.id)?;
    for _ in 0..2 {
        let options = TurnOptions::new(&CancelToken::new());
        let result = live
            .agent
            .run_turn(Some(&live.scope), id, TurnInput::text(&prompt), options)
            .await;
        match result {
            Ok(result) if result.exit_reason == ExitReason::Completed => {}
            Ok(result) => {
                ending.error = Some(result.error.unwrap_or_else(|| {
                    Error::new(
                        &codes::KANBAN_NOT_PERMITTED,
                        result.exit_reason.as_str().to_owned(),
                    )
                }));
                break;
            }
            Err(err) => {
                ending.error = Some(err);
                break;
            }
        }
        if !open(board, &row.id) {
            break;
        }
        NUDGE.clone_into(&mut prompt);
    }
    let reason = if ending.error.is_none() {
        EndReason::UserExit
    } else {
        EndReason::Error
    };
    live.close(&store, reason).await;
    Ok(())
}

/// Whether `run` is still open.
fn open(board: &Board, run: &str) -> bool {
    board.run(run).is_ok_and(|r| r.is_open())
}

/// Nobody answers a worker's secret prompts.
struct NoPrompt;

impl pmt_skills::secrets::SecretPrompt for NoPrompt {
    fn ask(&self, _: &str, _: &str, _: Option<&str>) -> Option<pmt_core::secret::SecretString> {
        None
    }
}

/// An `eval` task: the bundled `smoke` suite, run offline on the mock
/// provider by this binary (no live provider request in either
/// provider mode), its report attached and its summary the result.
fn eval(board: &mut Board, row: &Run, task: &Task, suite: &str, workspace: &Path) -> Ending {
    let failed = |message: String| Ending {
        error: Some(Error::new(&codes::KANBAN_NOT_PERMITTED, message)),
        session: None,
    };
    if suite != "smoke" {
        return failed(format!(
            "eval suite '{suite}' needs `prometheus eval run`, which this build does not have (only the bundled 'smoke' suite runs)"
        ));
    }
    let Ok(exe) = std::env::current_exe() else {
        return failed("cannot locate this executable".to_owned());
    };
    let out = workspace.join("eval");
    let _ = std::fs::remove_dir_all(&out);
    let output = std::process::Command::new(exe)
        .args(["smoke-suite", "--output"])
        .arg(&out)
        .output();
    let output = match output {
        Ok(output) => output,
        Err(e) => return failed(format!("cannot run the smoke suite: {e}")),
    };
    let report = String::from_utf8_lossy(&output.stdout).into_owned();
    let summary = report
        .lines()
        .find(|l| l.starts_with("smoke:"))
        .unwrap_or("smoke: no summary")
        .to_owned();
    let actor = worker_actor(row);
    let now = pmt_kanban::now_ms();
    let upload = pmt_kanban::attach::Upload {
        name: "eval-report.txt",
        content_type: Some("text/plain"),
        bytes: report.as_bytes(),
    };
    let limit = u64::MAX;
    if let Err(err) = board.attach(&actor, &task.id, &upload, limit, now) {
        return Ending {
            error: Some(err),
            session: None,
        };
    }
    let completion = pmt_kanban::Completion {
        result: Some(summary),
        attachment_limit: limit,
        ..pmt_kanban::Completion::default()
    };
    let error = board.complete(&actor, &task.id, &completion, now).err();
    Ending {
        error,
        session: None,
    }
}

/// The exit code a still-open run leaves for the dispatcher.
fn exit_code(error: Option<&Error>) -> i32 {
    let Some(err) = error else {
        return 0;
    };
    let text = err.message().to_ascii_lowercase();
    if text.contains("rate_limit") || text.contains("rate limit") || text.contains("429") {
        return EXIT_RATE_LIMITED;
    }
    match err.kind() {
        ErrorKind::Auth | ErrorKind::Config | ErrorKind::PermissionDenied => EXIT_TERMINAL,
        _ => 1,
    }
}

/// Records spend, blocks an exhausted budget, and leaves an open run's
/// exit code for the dispatcher.
fn finish(
    board: &mut Board,
    row: &Run,
    task: &Task,
    scope: &ProfileScope,
    ending: &Ending,
) -> Result<u8, Error> {
    let (cost, tokens) = ending
        .session
        .as_deref()
        .and_then(|id| id.parse().ok())
        .and_then(|id| {
            Store::open(scope)
                .ok()?
                .usage_totals(UsageScope::Lineage(id))
                .ok()
        })
        .map_or((0.0, 0), |t| {
            let tokens =
                t.tokens.input + t.tokens.cache_read + t.tokens.cache_write + t.tokens.output;
            (t.cost_usd, tokens)
        });
    board.record_usage(&row.id, ending.session.as_deref(), cost, tokens)?;
    if !open(board, &row.id) {
        return Ok(0);
    }
    let (spent_usd, spent_tokens) = board.spend(&task.id)?;
    let budget_hit = ending
        .error
        .as_ref()
        .is_some_and(|e| e.kind() == ErrorKind::BudgetExceeded)
        || task.budget_usd.is_some_and(|b| spent_usd >= b)
        || task.budget_tokens.is_some_and(|b| spent_tokens >= b);
    if budget_hit {
        let reason = match task.budget_usd {
            Some(limit) => format!("budget exhausted: spent {spent_usd:.2} of {limit:.2} USD"),
            None => format!(
                "budget exhausted: spent {spent_tokens} of {} tokens",
                task.budget_tokens.unwrap_or(0)
            ),
        };
        board.block(
            &worker_actor(row),
            &task.id,
            BlockKind::Budget,
            &reason,
            pmt_kanban::now_ms(),
        )?;
        return Ok(0);
    }
    let code = exit_code(ending.error.as_ref());
    let message = ending.error.as_ref().map(|e| e.message().to_owned());
    board.worker_exit(&row.id, code, message.as_deref())?;
    Ok(u8::try_from(code).unwrap_or(1))
}
