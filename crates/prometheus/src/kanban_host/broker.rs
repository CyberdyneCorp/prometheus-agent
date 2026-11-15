//! The broker of a worker lane (spec: kanban, Worker lanes):
//! `prometheus -p <dispatcher> kanban lane --run <id>`, started by the
//! dispatcher like a profile worker (scrubbed environment, run ticket on
//! stdin, its own process group). It checks the ticket, prepares the
//! task's workspace, starts the lane under the sandbox in its process group
//! and, until the lane exits, answers the lane's mailbox: each request is
//! checked against the run's ticket and applied here, outside the sandbox,
//! so the lane itself never holds write access to the board.

use std::io::BufRead as _;
use std::path::Path;
use std::time::Duration;

use clap::Parser as _;
use pmt_core::error::{Error, codes};
use pmt_kanban::ticket::Ticket;
use pmt_kanban::{BlockKind, Boards};
use serde_json::Value;

use super::mailbox::{self, Request};
use crate::context::Context;
use crate::kanban_cmd::ticketed::{self, Files};

/// How often the mailbox is read while the lane runs.
const POLL: Duration = Duration::from_millis(50);

fn denied(message: impl Into<String>) -> Error {
    Error::new(&codes::KANBAN_NOT_PERMITTED, message)
}

/// `kanban lane --run <run>`: runs the lane of `run` and serves its
/// mailbox until it exits.
///
/// # Errors
///
/// `kanban.ticket_refused`, the lane's start failure (the run then ends
/// as its worker's exit does), store errors.
pub fn serve(ctx: &Context<'_>, run: &str) -> Result<u8, Error> {
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    let ticket = Ticket::parse(&line, run)?;
    let scope = ctx.scope()?;
    crate::startup::bind_profile(&scope);
    let home = scope.home().to_path_buf();
    let mut board = Boards::open(&home)?.board(&ticket.board)?;
    let row = board.authenticate(run, &ticket.secret)?;
    let task = board.task(&row.task_id)?;
    let workspace = super::worker::prepare(&board, &task, &scope)
        .map_err(|e| Error::new(&codes::KANBAN_INVALID_TASK, e))?;
    board.set_workspace_path(&task.id, &workspace)?;
    drop(board);
    let dir = mailbox::dir(&home, run);
    pmt_core::private_fs::create_dir(&dir).map_err(|e| Error::io(&codes::HOME_IO, &dir, &e))?;
    let env = super::spawn::worker_env(ctx.launch, &home);
    let user_home = ctx.launch.user_home().cloned().unwrap_or_default();
    let start = super::lanes::Start {
        lane: &row.profile,
        board: &ticket.board,
        task: &task.id,
        run,
        workspace: &workspace,
        mailbox: &dir,
        env: &env,
        user_home: &user_home,
    };
    let limit = scope.config().kanban.attachment_max_bytes;
    let mut child = match super::lanes::spawn(&scope, &start, &line) {
        Ok(child) => child,
        Err(reason) => {
            // As a profile worker that cannot run its task: blocked with
            // kind `capability`, naming why.
            let _ = std::fs::remove_dir_all(&dir);
            let mut board = Boards::open(&home)?.board(&ticket.board)?;
            let actor = super::worker::worker_actor(&row);
            let now = pmt_kanban::now_ms();
            board.block(&actor, &task.id, BlockKind::Capability, &reason, now)?;
            return Ok(0);
        }
    };
    let mut handle = |request: &Request, at: &Path| answer(&home, &ticket, limit, request, at);
    let status = loop {
        mailbox::serve(&dir, &mut handle);
        match child.try_wait() {
            Ok(None) => std::thread::sleep(POLL),
            Ok(Some(status)) => break Ok(status),
            Err(err) => break Err(err),
        }
    };
    mailbox::serve(&dir, &mut handle);
    let _ = std::fs::remove_dir_all(&dir);
    let mut board = Boards::open(&home)?.board(&ticket.board)?;
    if board.run(run).is_ok_and(|r| r.is_open()) {
        // The lane ended without a terminator: its exit is the run's, as a
        // profile worker records its own.
        let (code, message) = lane_exit(&status);
        board.worker_exit(run, code, Some(&message))?;
    }
    Ok(0)
}

/// The exit code and message a lane that left its run open ends it with.
fn lane_exit(status: &std::io::Result<std::process::ExitStatus>) -> (i32, String) {
    let status = match status {
        Ok(status) => status,
        Err(err) => return (1, format!("lane exit unknown: {err}")),
    };
    if let Some(code) = status.code() {
        let message = if code == 0 {
            "lane exited without kanban complete, block or request-review".to_owned()
        } else {
            format!("lane exited with code {code}")
        };
        return (code, message);
    }
    (1, format!("lane ended by {status}"))
}

/// One lane request: a `prometheus kanban <verb> --ticket` command line
/// whose ticket is this run's.
fn answer(
    home: &Path,
    own: &Ticket,
    limit: u64,
    request: &Request,
    mailbox: &Path,
) -> Result<Value, Error> {
    let ticket = Ticket::read(&request.ticket)?;
    if ticket.run != own.run || ticket.board != own.board {
        return Err(denied(format!(
            "capability denied: this lane works run {}",
            own.run
        )));
    }
    let command_line = std::iter::once("prometheus".to_owned()).chain(request.argv.iter().cloned());
    let cli = crate::cli::Cli::try_parse_from(command_line)
        .map_err(|e| denied(format!("invalid lane command: {e}")))?;
    let Some(crate::cli::Command::Kanban(args)) = cli.command else {
        return Err(denied(
            "a lane runs only `prometheus kanban <verb> --ticket`",
        ));
    };
    if args.ticket.is_none() {
        return Err(denied(
            "a lane runs only `prometheus kanban <verb> --ticket`",
        ));
    }
    let files = Files::Staged {
        dir: mailbox,
        names: &request.staged,
        limit,
    };
    ticketed::answer(&args, home, &ticket, &files)
}
