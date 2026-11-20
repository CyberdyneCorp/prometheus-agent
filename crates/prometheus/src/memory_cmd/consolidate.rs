//! `prometheus memory consolidate` and the daemon's idle-time pass (spec:
//! memory, Consolidation proposals). The auxiliary memory model proposes;
//! `pmt_memory::consolidate` decides what may change. The CLI prints the
//! diff and commits only with `--apply`; the idle pass commits with
//! `memory.consolidation.auto_apply`, otherwise stages the change for
//! `memory approve`.

use std::path::PathBuf;
use std::sync::Arc;

use jiff::Timestamp;
use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_core::error::Error;
use pmt_memory::consolidate::{self, Plan};
use pmt_providers::auxiliary::AuxTask;
use pmt_store::model::Source;
use pmt_store::sessions::NewSession;
use pmt_store::{MemoryTarget, Store};
use serde_json::json;

use super::Env;
use crate::chat::session::model::{Models, RouteFlags};
use crate::output::{json, print_raw};

/// What a pass does with its plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Show it only.
    Show,
    /// Commit it.
    Apply,
    /// Stage it for approval.
    Stage,
}

/// One target's proposal and what became of it.
#[derive(Debug)]
pub(crate) struct Outcome {
    pub(crate) plan: Plan,
    pub(crate) diff: String,
    pub(crate) pending: Option<String>,
}

/// Asks the model about each of `targets` and handles the plan per `mode`.
pub(crate) fn pass(
    scope: &ProfileScope,
    store: &Arc<Store>,
    launch: &LaunchEnv,
    targets: &[MemoryTarget],
    mode: Mode,
) -> Result<Vec<Outcome>, Error> {
    let config = &scope.config().memory;
    let now = Timestamp::now();
    let mut models = None;
    let mut out = Vec::new();
    for &target in targets {
        let entries = store.memory_entries(Some(target))?;
        let prompt = consolidate::prompt(&entries, config, target);
        if !prompt.contains("- id=") {
            continue;
        }
        if models.is_none() {
            models = Some(Models::open(
                scope,
                store,
                launch.user_home().map(PathBuf::as_path),
                &RouteFlags::default(),
                None,
            )?);
        }
        let Some(models) = models.as_ref() else {
            continue;
        };
        // Untitled: a hidden accounting session per call (titles are unique).
        let session = store.create_session(&NewSession {
            source_tag: Some("memory-consolidation".to_owned()),
            hidden: true,
            ..NewSession::new(Source::Cli)
        })?;
        let reply = crate::model_cmd::block_on(models.ask_auxiliary(
            AuxTask::Memory,
            session.id,
            consolidate::INSTRUCTIONS,
            &prompt,
            4_000,
        ))??;
        let proposals = consolidate::parse(reply.as_deref().unwrap_or("{\"changes\": []}"))?;
        let plan = consolidate::plan(&entries, target, &proposals);
        let diff = consolidate::preview(store, config, &plan, now)?;
        let pending = match mode {
            Mode::Show => None,
            Mode::Apply => {
                consolidate::commit(store, config, &plan, now)?;
                None
            }
            Mode::Stage => consolidate::stage(store, config, &plan, now)?,
        };
        out.push(Outcome {
            plan,
            diff,
            pending,
        });
    }
    Ok(out)
}

/// `memory consolidate`.
pub(super) fn run(
    env: &Env,
    launch: &LaunchEnv,
    targets: &[MemoryTarget],
    apply: bool,
) -> Result<u8, Error> {
    let mode = if apply { Mode::Apply } else { Mode::Show };
    let outcomes = pass(&env.scope, &env.store, launch, targets, mode)?;
    if env.json {
        let rows: Vec<_> = outcomes
            .iter()
            .map(|o| {
                json!({
                    "target": o.plan.target,
                    "operations": o.plan.ops,
                    "skipped": o.plan.skipped,
                    "diff": o.diff,
                    "applied": apply && !o.plan.is_empty(),
                })
            })
            .collect();
        return json(&rows);
    }
    let mut text = String::new();
    for o in &outcomes {
        for skipped in &o.plan.skipped {
            eprintln!("skipped {skipped}");
        }
        text.push_str(&o.diff);
    }
    if text.is_empty() {
        eprintln!("nothing to consolidate");
        return Ok(0);
    }
    print_raw(&text)?;
    if apply {
        eprintln!("applied (origin consolidation; from the next session)");
    } else {
        eprintln!("nothing was changed; run with --apply to commit this");
    }
    Ok(0)
}

/// The daemon's idle-time pass for one profile: runs on its own thread
/// when `memory.consolidation.enabled` and the interval has passed.
pub(crate) fn check(
    scope: &ProfileScope,
    store: &Arc<Store>,
    launch: &LaunchEnv,
) -> Option<std::thread::JoinHandle<()>> {
    let now = Timestamp::now();
    let last_activity = store.profile_last_activity().ok().flatten();
    match consolidate::due(store, &scope.config().memory, now, last_activity) {
        Ok(true) => {}
        Ok(false) => return None,
        Err(e) => {
            tracing::warn!(error = %e, "memory consolidation schedule check failed");
            return None;
        }
    }
    if let Err(e) = consolidate::mark_run(store, now) {
        tracing::warn!(error = %e, "memory consolidation not started");
        return None;
    }
    let (scope, store, launch) = (scope.clone(), Arc::clone(store), launch.clone());
    std::thread::Builder::new()
        .name("pmt-memory-consolidation".to_owned())
        .spawn(move || {
            let mode = if scope.config().memory.consolidation.auto_apply {
                Mode::Apply
            } else {
                Mode::Stage
            };
            let targets = pmt_memory::budget::enabled(&scope.config().memory);
            match pass(&scope, &store, &launch, &targets, mode) {
                Ok(outcomes) => {
                    let staged: Vec<String> =
                        outcomes.into_iter().filter_map(|o| o.pending).collect();
                    tracing::info!(?mode, ?staged, "memory consolidation pass");
                }
                Err(e) => tracing::warn!(error = %e, "memory consolidation pass failed"),
            }
        })
        .ok()
}
