//! What rollouts and improver sessions share: the job's root session
//! (every rollout and improver session is its hidden child, so the job's
//! spend sums under one root in the cost ledger), a turn bounded by a
//! wall-clock limit and the job's cancel token, and the trajectory and
//! spend read back from the store.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pmt_agent::turn::events::NoEvents;
use pmt_agent::turn::{EventSink, ExitReason, Issuer, TurnInput, TurnOptions, TurnResult};
use pmt_core::cancel::CancelToken;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_core::secret::SecretString;
use pmt_evolve::engine::ports::{Entry, EntryKind};
use pmt_skills::secrets::SecretPrompt;
use pmt_store::{EndReason, LineageKind, NewSession, Role, Source, Store, UsageScope, Visibility};

use crate::chat::session::{Clients, Factory, Live};

/// The source tag of rollout sessions.
pub const ROLLOUT_TAG: &str = "evolve-rollout";
/// The source tag of improver sessions.
pub const IMPROVER_TAG: &str = "evolve-improver";

/// Nobody answers a rollout's or improver's secret prompts.
struct NoPrompt;

impl SecretPrompt for NoPrompt {
    fn ask(&self, _: &str, _: &str, _: Option<&str>) -> Option<SecretString> {
        None
    }
}

/// Clients that see nothing and answer nothing.
#[must_use]
pub fn detached() -> Clients {
    Clients {
        events: Arc::new(NoEvents) as Arc<dyn EventSink>,
        secrets: Arc::new(NoPrompt),
        completions: None,
        sudo: None,
        delegation: None,
    }
}

/// The hidden root session of job `job_id` (`<job>/root_session`),
/// created on first use.
///
/// # Errors
///
/// Store errors.
pub fn job_root(store: &Store, job_dir: &Path, job_id: &str) -> Result<SessionId, Error> {
    let file = job_dir.join("root_session");
    if let Some(id) = std::fs::read_to_string(&file)
        .ok()
        .and_then(|t| SessionId::parse(t.trim()).ok())
        .filter(|id| store.session(*id).is_ok())
    {
        return Ok(id);
    }
    let root = store.create_session(&NewSession {
        source_tag: Some("evolve-job".to_owned()),
        title: Some(format!("evolution job {job_id}")),
        hidden: true,
        ..NewSession::new(Source::Harness)
    })?;
    let _ = std::fs::write(&file, root.id.to_string());
    Ok(root.id)
}

/// A hidden child session of `root` for `factory`'s route.
///
/// # Errors
///
/// Store errors.
pub fn child_row(
    store: &Store,
    root: SessionId,
    tag: &str,
    target: &pmt_providers::resolve::Target,
    cwd: &Path,
) -> Result<SessionId, Error> {
    let mut new = NewSession::child(Source::Harness, root, LineageKind::Delegate);
    new.source_tag = Some(tag.to_owned());
    new.hidden = true;
    new.provider = Some(target.provider.clone());
    new.model.clone_from(&target.model);
    new.cwd = Some(cwd.to_string_lossy().into_owned());
    Ok(store.create_session(&new)?.id)
}

/// Runs one turn of `live` with `text`, stopped by `cancel` or after
/// `limit`; the session is closed either way.
///
/// # Errors
///
/// The turn's error, or `task.cancelled` past `limit`.
pub async fn turn(
    factory: &Factory,
    live: Live,
    text: &str,
    cancel: &CancelToken,
    limit: Duration,
) -> Result<TurnResult, Error> {
    let options = TurnOptions::new(cancel);
    let interrupt = options.interrupt.clone();
    let id = live.id;
    let result = {
        let run = live
            .agent
            .run_turn(Some(&live.scope), id, TurnInput::text(text), options);
        tokio::pin!(run);
        tokio::select! {
            result = &mut run => result,
            () = tokio::time::sleep(limit) => {
                interrupt.stop(Issuer::System("evolve_timeout".to_owned()));
                let _ = tokio::time::timeout(Duration::from_secs(10), &mut run).await;
                Err(Error::new(
                    &pmt_core::error::codes::TASK_CANCELLED,
                    format!("timed out after {} s", limit.as_secs()),
                ))
            }
        }
    };
    let reason = match &result {
        Ok(r) if r.exit_reason == ExitReason::Completed => EndReason::UserExit,
        _ => EndReason::Error,
    };
    live.close(&factory.store, reason).await;
    result
}

/// The final assistant text of a turn that completed.
///
/// # Errors
///
/// Why there is none.
pub fn final_text(result: &Result<TurnResult, Error>) -> Result<String, String> {
    match result {
        Ok(r) if r.exit_reason == ExitReason::Completed => {
            Ok(r.final_response.clone().unwrap_or_default())
        }
        Ok(r) => Err(match &r.error {
            Some(err) => format!("{}: {}", r.exit_reason.as_str(), err.message()),
            None => r.exit_reason.as_str().to_owned(),
        }),
        Err(err) => Err(err.message().to_owned()),
    }
}

/// Session `id`'s spend in US dollars.
#[must_use]
pub fn spend(store: &Store, id: SessionId) -> f64 {
    store
        .usage_totals(UsageScope::Session(id))
        .map(|t| t.cost_usd)
        .unwrap_or_default()
}

/// Session `id`'s trajectory as engine entries.
#[must_use]
pub fn entries(store: &Store, id: SessionId) -> Vec<Entry> {
    let Ok(messages) = store.messages(id, Visibility::All) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for message in messages {
        let text = message.content.clone().unwrap_or_default();
        match message.role {
            Role::User => out.push(Entry::new(EntryKind::User, text)),
            Role::Assistant => {
                if !text.trim().is_empty() {
                    out.push(Entry::new(EntryKind::Assistant, text));
                }
                for call in message
                    .tool_calls
                    .as_ref()
                    .and_then(|c| c.as_array())
                    .into_iter()
                    .flatten()
                {
                    let name = call["name"].as_str().unwrap_or("?").to_owned();
                    let arguments = match &call["arguments"] {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    out.push(Entry::new(EntryKind::ToolCall, arguments).tool(name));
                }
            }
            Role::Tool => out.push(
                Entry::new(EntryKind::ToolResult, text)
                    .tool(message.tool_name.clone().unwrap_or_default()),
            ),
            Role::System => {}
        }
    }
    out
}
