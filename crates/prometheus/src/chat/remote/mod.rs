//! `prometheus chat` as a client of the home's running daemon (spec: cli,
//! Daemon attachment with in-process fallback; Single interactive owner
//! per session; workspace-architecture, Clients attach to a running
//! daemon). The daemon owns the session store, so an attached chat never
//! opens `state.db`: it creates or resumes the session over RPC, submits
//! prompts, renders the session's events and answers its approval and
//! clarify requests at the terminal.
//!
//! The session-shaping flags (`-m`, `--provider`, `--set`, `--toolsets`,
//! `--skills`, `--approvals`, `--yolo`, `--max-iterations`, ...) travel as
//! the session's setup in `session.create`/`session.resume`; `--image`
//! and `--run-budget` go with the prompts; `--usage-file` is written here
//! from the turns' reports.
//!
//! The session is opened exclusively: while this chat is attached no other
//! interactive client may resume it; one that tries is offered to follow
//! it read-only or to fork it.

mod line;
mod local;
mod oneshot;
mod open;
mod render;
pub(crate) mod turn;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_rpc::client::{Client, Inbox};
use pmt_rpc::contracts::{PromptImage, SessionSetup};
use pmt_rpc::discovery::DaemonRecord;

use super::args::ChatArgs;
use super::usage_file::RunRecord;
use super::{Mode, Ui};

/// An attached connection and the session it drives.
pub(super) struct Attached {
    pub(super) client: Client,
    pub(super) inbox: Inbox,
    pub(super) session: String,
    /// The daemon's pid, for messages.
    pub(super) daemon_pid: u32,
    /// `--run-budget` seconds, per turn.
    pub(super) run_budget: Option<u64>,
    /// What the run did (`--usage-file`).
    pub(super) record: Arc<RunRecord>,
    /// The session's live goal, loop and heartbeat rows by kind, from its
    /// `automation.state` events (the line interface's status line).
    pub(super) automation: std::collections::BTreeMap<String, String>,
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
    /// How the daemon runs the session (the chat's flags).
    pub setup: SessionSetup,
    /// `--image` files (checked already) for the first prompt.
    pub images: Vec<PathBuf>,
    /// `--run-budget` seconds, per turn.
    pub run_budget: Option<u64>,
    /// What the run did (`--usage-file`).
    pub record: Arc<RunRecord>,
    /// The client's configuration (display preferences).
    pub scope: &'a ProfileScope,
    /// The launch environment (`$EDITOR`, `NO_COLOR`).
    pub launch: &'a pmt_core::env::LaunchEnv,
}

impl Start<'_> {
    /// The `--image` files as prompt attachments (absolute, for the
    /// daemon, which may work elsewhere).
    pub(super) fn first_images(&self) -> Vec<PromptImage> {
        self.images
            .iter()
            .map(|path| PromptImage::Path(absolute(self.cwd, path)))
            .collect()
    }
}

/// `path` resolved against `cwd`.
fn absolute(cwd: &Path, path: &Path) -> String {
    cwd.join(path).to_string_lossy().into_owned()
}

/// The session setup of a chat's flags: `-m`/`--provider`, every `--set`
/// (the global ones and those the chat flags stand for), the switches and
/// the ad-hoc endpoint (its key read from stdin when asked).
///
/// # Errors
///
/// `--api-key-stdin` cannot read the key.
pub fn setup_of(args: &ChatArgs, set: &[String], cwd: &Path) -> Result<SessionSetup, Error> {
    let api_key = if args.endpoint.api_key_stdin {
        Some(
            crate::prompt::read_secret("the --base-url API key")?
                .expose()
                .to_owned(),
        )
    } else {
        None
    };
    Ok(SessionSetup {
        model: args.model.clone(),
        provider: args.provider.clone(),
        set: set.to_vec(),
        skills: args.skills.clone(),
        yolo: args.yolo,
        pass_session_id: args.pass_session_id,
        no_context_files: args.no_context_files,
        ignore_user_config: args.ignores_user_config(),
        ignore_rules: args.ignores_rules(),
        mock_script: args.mock_script.as_deref().map(|p| absolute(cwd, p)),
        base_url: args.endpoint.base_url.clone(),
        api_key,
    })
}

/// How long an exiting client waits for the daemon to take its close.
const CLOSE_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Runs the chat attached to `record`'s daemon.
///
/// # Errors
///
/// `home.locked` (75) when the daemon does not serve the profile,
/// connection and session errors.
pub fn run(record: &DaemonRecord, scope: &ProfileScope, start: &Start<'_>) -> Result<u8, Error> {
    crate::model_cmd::block_on(async {
        let hello = crate::daemon_client::hello(pmt_rpc::contracts::ClientKind::Cli, true);
        let (client, inbox) =
            crate::daemon_client::connect_cli(record, scope.name().as_str(), &hello)
                .await
                .map_err(|err| not_served(&err, record, scope))?;
        let mut attached = Attached {
            client,
            inbox,
            session: String::new(),
            daemon_pid: record.process.pid,
            run_budget: start.run_budget,
            record: Arc::clone(&start.record),
            automation: std::collections::BTreeMap::new(),
        };
        let status = match open_session(&mut attached, start).await? {
            Opened::Session(session) => {
                crate::egress::send(&attached.client, &session, &start.args.egress_overrides())
                    .await?;
                attached.session = session;
                match start.mode {
                    Mode::OneShot => oneshot::run(&mut attached, start).await,
                    // A TUI run never gets here: it opened the TUI before.
                    Mode::Line | Mode::Tui => line::run(&mut attached, start).await,
                }
            }
            Opened::Followed(status) => Ok(status),
        };
        attached.client.close();
        // The daemon releases the session's ownership when it sees the
        // close: a client started right after this one exits (`-r` of the
        // same session) must not be refused by one that is gone.
        let _ = tokio::time::timeout(CLOSE_WAIT, attached.client.wait_closed()).await;
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
            let fork = open::fork(client, &session, &start.setup).await?;
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
