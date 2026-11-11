//! Which session an attached chat drives: a new one, or the one `-r`/`-c`
//! names (resolved by the daemon, which owns the store), always opened as
//! its exclusive interactive owner. A session another client owns is
//! offered to follow read-only or to fork (spec: cli, Single interactive
//! owner per session).

use std::io::Write as _;

use pmt_core::error::Error;
use pmt_rpc::client::{Client, error_of};
use pmt_rpc::contracts::{
    AttachParams, AttachRole, ForkParams, ResolveKind, ResolveParams, ResumeParams, SessionAttach,
    SessionCreate, SessionCreateParams, SessionFork, SessionRef, SessionResolve, SessionResume,
    SessionSetup,
};

use super::Start;

/// The stored session `-r`/`-c` names; `None` for a new session.
///
/// # Errors
///
/// The daemon's resolution errors (`session.not_found`,
/// `chat.nothing_to_continue`, `session.ambiguous`...).
pub(super) async fn target(client: &Client, start: &Start<'_>) -> Result<Option<String>, Error> {
    let args = start.args;
    let (kind, reference) = match (&args.resume, &args.continue_) {
        (Some(reference), _) => (ResolveKind::Resume, reference.clone()),
        (None, Some(name)) => (ResolveKind::Continue, name.clone()),
        (None, None) => return Ok(None),
    };
    let params = ResolveParams {
        profile: None,
        kind,
        reference,
        cwd: Some(start.cwd.to_string_lossy().into_owned()),
        terminal: start.terminal.clone(),
    };
    let found = client
        .call_typed::<SessionResolve>(&params)
        .await
        .map_err(|e| error_of(&e))?;
    Ok(Some(found.session_id))
}

/// Creates the session, or resumes `target`, as its exclusive owner.
///
/// # Errors
///
/// `session.in_use` (75) when another client owns `target`; creation and
/// resume errors.
pub(super) async fn claim(
    client: &Client,
    target: Option<&str>,
    start: &Start<'_>,
) -> Result<String, Error> {
    if let Some(session_id) = target {
        resume(client, session_id, &start.setup).await?;
        return Ok(session_id.to_owned());
    }
    let args = start.args;
    let source_tag = args.source.clone().or_else(|| {
        (start.mode == super::Mode::OneShot).then(|| pmt_store::sessions::ONESHOT_TAG.to_owned())
    });
    let params = SessionCreateParams {
        profile: None,
        cwd: Some(start.cwd.to_string_lossy().into_owned()),
        source_tag,
        source: None,
        idempotency_key: None,
        exclusive: true,
        setup: start.setup.clone(),
    };
    let created: SessionRef = client
        .call_typed::<SessionCreate>(&params)
        .await
        .map_err(|e| error_of(&e))?;
    Ok(created.session_id)
}

/// Resumes `session_id` as its owner, with `setup` when the chat's flags
/// ask for one (otherwise the session runs as it is).
pub(super) async fn resume(
    client: &Client,
    session_id: &str,
    setup: &SessionSetup,
) -> Result<(), Error> {
    let params = ResumeParams {
        session_id: session_id.to_owned(),
        exclusive: true,
        setup: (*setup != SessionSetup::default()).then(|| setup.clone()),
    };
    client
        .call_typed::<SessionResume>(&params)
        .await
        .map(|_| ())
        .map_err(|e| error_of(&e))
}

/// What the person chose for a session another client owns.
pub(super) enum Choice {
    /// Fork it and drive the fork.
    Fork,
    /// Follow it read-only.
    Follow,
    /// Give up (the ownership error stands).
    Cancel,
}

/// Prints the ownership error and asks whether to follow the session
/// read-only or fork it.
///
/// # Errors
///
/// `output.io` when the terminal cannot be read.
pub(super) async fn offer(owned: &Error) -> Result<Choice, Error> {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", owned.message());
    let _ = write!(out, "[f]ollow it read-only, fork it [k], or [c]ancel? ");
    let _ = out.flush();
    drop(out);
    let answer = pmt_core::spawn::blocking(|| {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map(|_| line)
    })
    .await
    .map_err(|e| read_error(&e))?
    .map_err(|e| read_error(&e))?;
    Ok(match answer.trim().to_lowercase().as_str() {
        "f" | "follow" => Choice::Follow,
        "k" | "fork" => Choice::Fork,
        _ => Choice::Cancel,
    })
}

fn read_error(e: &dyn std::fmt::Display) -> Error {
    Error::new(
        &pmt_core::error::codes::OUTPUT_IO,
        format!("cannot read the terminal: {e}"),
    )
}

/// Forks `session` at its last message and opens the fork as its owner.
///
/// # Errors
///
/// Fork and resume errors.
pub(super) async fn fork(
    client: &Client,
    session: &str,
    setup: &SessionSetup,
) -> Result<String, Error> {
    // Forking reads the parent: attach to it read-only first.
    let observe = AttachParams {
        role: AttachRole::Observer,
        ..AttachParams::driver(session)
    };
    client
        .call_typed::<SessionAttach>(&observe)
        .await
        .map_err(|e| error_of(&e))?;
    let params = ForkParams {
        session_id: session.to_owned(),
        at_message_id: None,
        title: None,
    };
    let forked = client
        .call_typed::<SessionFork>(&params)
        .await
        .map_err(|e| error_of(&e))?;
    resume(client, &forked.session_id, setup).await?;
    Ok(forked.session_id)
}
