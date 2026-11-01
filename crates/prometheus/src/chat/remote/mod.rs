//! `prometheus chat` as a client of the home's running daemon (spec: cli,
//! Daemon attachment with in-process fallback; Single interactive owner
//! per session; workspace-architecture, Clients attach to a running
//! daemon). The daemon owns the session store, so an attached chat never
//! opens `state.db`: it creates or resumes the session over RPC, submits
//! prompts, renders the session's events and answers its approval and
//! clarify requests at the terminal.
//!
//! The session is opened exclusively: while this chat is attached no other
//! interactive client may resume it; one that tries is offered to follow
//! it read-only or to fork it.

mod line;
mod oneshot;
mod open;
mod render;
mod turn;

use std::path::Path;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_rpc::client::{Client, Inbox};
use pmt_rpc::discovery::DaemonRecord;

use super::args::ChatArgs;
use super::{Mode, Ui};

/// An attached connection and the session it drives.
pub(super) struct Attached {
    pub(super) client: Client,
    pub(super) inbox: Inbox,
    pub(super) session: String,
    /// The daemon's pid, for messages.
    pub(super) daemon_pid: u32,
}

/// What the attached chat starts with.
pub struct Start<'a> {
    /// The flags.
    pub args: &'a ChatArgs,
    /// One-shot or the line interface.
    pub mode: Mode,
    /// The first prompt (one-shot: the prompt; line: `--query` or the
    /// prompt file).
    pub first: Option<String>,
    /// The session's working directory.
    pub cwd: &'a Path,
    /// This terminal's identity (`-c` prefers its last session).
    pub terminal: Option<String>,
    /// Output flags.
    pub ui: Ui,
    /// `approvals.single_query_mode` approves (one-shot runs).
    pub approve_unattended: bool,
    /// `--set` overrides were given.
    pub overrides: bool,
}

/// Runs the chat attached to `record`'s daemon.
///
/// # Errors
///
/// `cli.usage` (2) for a flag the daemon cannot honour, `rpc.ticket` when
/// the daemon does not serve the profile, connection and session errors.
pub fn run(record: &DaemonRecord, scope: &ProfileScope, start: &Start<'_>) -> Result<u8, Error> {
    refuse_local_flags(start.args, start.overrides, record.process.pid)?;
    crate::model_cmd::block_on(async {
        let hello = crate::daemon_client::hello(true);
        let (client, inbox) = crate::daemon_client::connect(record, scope.name().as_str(), &hello)
            .await
            .map_err(|err| not_served(&err, record, scope))?;
        let mut attached = Attached {
            client,
            inbox,
            session: String::new(),
            daemon_pid: record.process.pid,
        };
        let status = match open_session(&mut attached, start).await? {
            Opened::Session(session) => {
                attached.session = session;
                match start.mode {
                    Mode::OneShot => oneshot::run(&mut attached, start).await,
                    Mode::Line => line::run(&mut attached, start).await,
                }
            }
            Opened::Followed(status) => Ok(status),
        };
        attached.client.close();
        status
    })?
}

enum Opened {
    Session(String),
    Followed(u8),
}

/// The session to drive. A session another client owns is offered to
/// follow or fork on the line interface; a one-shot run fails (75).
async fn open_session(attached: &mut Attached, start: &Start<'_>) -> Result<Opened, Error> {
    let client = &attached.client;
    let target = open::target(client, start).await?;
    let owned = match open::claim(client, target.as_deref(), start).await {
        Ok(session) => return Ok(Opened::Session(session)),
        Err(err) if start.mode == Mode::Line && err.code().code == codes::SESSION_IN_USE.code => {
            err
        }
        Err(err) => return Err(err),
    };
    let session = target.unwrap_or_default();
    match open::offer(&owned).await? {
        open::Choice::Fork => {
            let fork = open::fork(client, &session).await?;
            println!("forked session {session} as {fork}");
            Ok(Opened::Session(fork))
        }
        open::Choice::Follow => {
            attached.session = session;
            line::follow(attached, start).await.map(Opened::Followed)
        }
        open::Choice::Cancel => Err(owned),
    }
}

/// The ticket refusal for a profile the daemon does not serve says what to
/// do: the daemon holds the home, so the chat cannot run in-process.
fn not_served(err: &Error, record: &DaemonRecord, scope: &ProfileScope) -> Error {
    if err.code().code != codes::RPC_TICKET.code {
        return Error::new(err.code(), err.message().to_owned());
    }
    Error::new(
        &codes::HOME_LOCKED,
        format!(
            "the gateway daemon (pid {}) owns this home but does not serve profile '{}'",
            record.process.pid,
            scope.name()
        ),
    )
    .with_hint(
        "serve it with 'gateway.multiplex = true' in the daemon's launch profile, or stop the daemon with 'prometheus daemon stop'",
    )
}

/// Flags that shape a session in this process (its configuration, tools
/// or attachments) cannot reach a session the daemon runs; refusing them
/// says so instead of dropping them.
fn refuse_local_flags(args: &ChatArgs, overrides: bool, daemon_pid: u32) -> Result<(), Error> {
    let given: &[(&str, bool)] = &[
        ("--set", overrides),
        ("--image", !args.image.is_empty()),
        ("--approvals", args.approvals.is_some()),
        ("--yolo", args.yolo),
        ("--max-iterations", args.max_iterations.is_some()),
        ("--toolsets", !args.toolsets.is_empty()),
        ("--skills", !args.skills.is_empty()),
        ("--pass-session-id", args.pass_session_id),
        ("--run-budget", args.run_budget.is_some()),
        ("--no-context-files", args.no_context_files),
        ("--ignore-user-config", args.ignore_user_config),
        ("--ignore-rules", args.ignore_rules),
        ("--usage-file", args.usage_file.is_some()),
        ("--mock-script", args.mock_script.is_some()),
        ("--base-url", args.endpoint.base_url.is_some()),
    ];
    let Some((flag, _)) = given.iter().find(|(_, on)| *on) else {
        return Ok(());
    };
    Err(Error::new(
        &codes::CLI_USAGE,
        format!(
            "{flag} cannot be used while the gateway daemon (pid {daemon_pid}) runs this home's sessions"
        ),
    )
    .with_hint("drop the flag, or stop the daemon with 'prometheus daemon stop' to run the chat in this process"))
}
