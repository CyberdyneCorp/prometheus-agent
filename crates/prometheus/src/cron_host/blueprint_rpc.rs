//! RPC `cron.blueprints.list` and `cron.blueprints.fill` (spec:
//! scheduled-jobs, Automation blueprint catalog): the same catalog and
//! slot checks as `/blueprint`, for the dashboard and desktop forms. A fill
//! answers the slot still missing, or the schedule echo; with `create` it
//! saves the job through the ordinary validated create path (no chat
//! origin, so it delivers `local` unless the blueprint says otherwise).

use std::sync::Arc;

use jiff::Timestamp;
use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_core::error::Error;
use pmt_cron::blueprint::{self, Blueprint, Fill, Origin, SlotKind, slots};
use pmt_cron::ops;
use pmt_rpc::contracts::{
    BlueprintFillParams, BlueprintFilled, BlueprintInfo, BlueprintList, BlueprintSlot,
};
use pmt_store::Store;

use super::{HostRunner, blueprints, scripts_dir};
use crate::cron_cmd::JobFlags;

fn kind(kind: &SlotKind) -> &'static str {
    match kind {
        SlotKind::Time => "time",
        SlotKind::Enum => "enum",
        SlotKind::Text => "text",
        SlotKind::Weekdays => "weekdays",
    }
}

fn info(b: &Blueprint) -> BlueprintInfo {
    BlueprintInfo {
        key: b.key.clone(),
        title: b.title.clone(),
        description: b.description.clone(),
        category: b.category.clone(),
        source: match b.origin {
            Origin::Builtin => "builtin",
            Origin::Plugin { .. } => "plugin",
            Origin::Skill { .. } => "skill",
        }
        .to_owned(),
        slots: b
            .slots
            .iter()
            .map(|s| BlueprintSlot {
                name: s.name.clone(),
                kind: kind(&s.kind).to_owned(),
                question: s.prompt(),
                options: if s.kind == SlotKind::Weekdays {
                    slots::WEEKDAY_PRESETS.map(str::to_owned).to_vec()
                } else {
                    s.options.clone()
                },
                strict: s.strict,
                default: s.default.clone(),
                required: s.required,
            })
            .collect(),
    }
}

/// `cron.blueprints.list`.
#[must_use]
pub fn list(scope: &ProfileScope, launch: &LaunchEnv) -> BlueprintList {
    BlueprintList {
        blueprints: blueprints::catalog(scope, launch)
            .iter()
            .map(info)
            .collect(),
    }
}

/// `cron.blueprints.fill`.
///
/// # Errors
///
/// `cron.blueprint` (unknown blueprint, invalid or unknown slot), the
/// errors of checking and creating the job.
pub fn fill(
    scope: &ProfileScope,
    launch: &LaunchEnv,
    store: Arc<Store>,
    params: &BlueprintFillParams,
) -> Result<BlueprintFilled, Error> {
    let catalog = blueprints::catalog(scope, launch);
    let chosen = blueprint::resolve(&catalog, &params.key)?;
    let filled = match chosen.fill(&params.values)? {
        Fill::Missing(slot) => {
            return Ok(BlueprintFilled {
                status: "missing".to_owned(),
                key: chosen.key.clone(),
                question: Some(slot.prompt()),
                missing: Some(slot.name),
                ..BlueprintFilled::default()
            });
        }
        Fill::Ready(filled) => filled,
    };
    let now = Timestamp::now();
    let runner = HostRunner::new(scope, launch, store);
    let scripts = scripts_dir(scope);
    let profile = runner.profile(&scripts);
    let flags = JobFlags {
        prompt: Some(filled.prompt.clone()),
        name: Some(filled.title.clone()),
        ..JobFlags::default()
    };
    let spec = flags
        .fields(Some(&filled.schedule), scope)?
        .new_spec(now, &profile.zone)?;
    let mut checked = spec.clone();
    if checked.deliver.is_empty() {
        checked.deliver = ops::default_deliver(None);
    }
    let echo = ops::check(&profile, &checked, now)?;
    let mut answer = BlueprintFilled {
        status: "ready".to_owned(),
        key: chosen.key.clone(),
        schedule: Some(format!("{} {}", echo.kind, echo.canonical)),
        echo: Some(echo.render()),
        prompt: Some(filled.prompt.clone()),
        ..BlueprintFilled::default()
    };
    if params.create {
        let new = ops::NewJob {
            name: Some(filled.title),
            origin: None,
            paused: None,
        };
        let job = ops::create(&profile, spec, new, now)?;
        "created".clone_into(&mut answer.status);
        answer.job_id = Some(job.id().to_owned());
    }
    Ok(answer)
}
