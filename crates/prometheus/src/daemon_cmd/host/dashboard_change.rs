//! The dashboard's changes to a served profile (`pmt_web::DashboardHost::
//! change`), each done by the same code as its CLI command: skill and
//! toolset switches, cron jobs, memory entries, webhook subscriptions and
//! bulk maintenance. A table maps each change to its handler.

use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_cron::ops;
use pmt_store::Store;
use pmt_web::Change;
use serde_json::{Value, json};

use super::Profile;
use crate::cron_host::{HostCatalog, profile_zone, scripts_dir};

/// What a handler works on.
pub(super) struct Target<'a> {
    pub launch: &'a LaunchEnv,
    pub profile: Arc<Profile>,
    pub scope: ProfileScope,
    pub store: Arc<Store>,
    pub key: Option<&'a str>,
    pub body: Value,
}

impl Target<'_> {
    fn key(&self) -> Result<&str, Error> {
        self.key
            .ok_or_else(|| Error::new(&codes::CLI_USAGE, "the change names no target"))
    }

    fn flag(&self, name: &str) -> Result<bool, Error> {
        self.body[name].as_bool().ok_or_else(|| {
            Error::new(
                &codes::CLI_USAGE,
                format!("expected {{\"{name}\": true|false}}"),
            )
        })
    }
}

type Handler = fn(&Target<'_>) -> Result<Value, Error>;

/// The synchronous handlers; webhook tests are sent asynchronously.
const HANDLERS: &[(Change, Handler)] = &[
    (Change::Skill, skill),
    (Change::SkillArchive, skill_archive),
    (Change::Toolset, toolset),
    (Change::CronCreate, cron_create),
    (Change::CronUpdate, cron_update),
    (Change::CronDelete, cron_delete),
    (Change::CronRun, cron_run),
    (Change::Memory, memory),
    (Change::WebhookCreate, webhook_create),
    (Change::WebhookUpdate, webhook_update),
    (Change::WebhookDelete, webhook_delete),
    (Change::Maintenance, maintenance),
];

/// Applies `change` to `target`.
pub(super) async fn apply(change: Change, target: Target<'_>) -> Result<Value, Error> {
    if change == Change::WebhookTest {
        let sent = crate::webhooks_cmd::send_test(&target.scope, target.key()?, None).await?;
        return Ok(json!({"delivery_id": sent.delivery, "status": sent.status}));
    }
    let handler = HANDLERS
        .iter()
        .find(|(c, _)| *c == change)
        .map(|(_, handler)| *handler)
        .ok_or_else(|| Error::new(&codes::CLI_USAGE, format!("unsupported change {change:?}")))?;
    handler(&target)
}

fn skill(t: &Target<'_>) -> Result<Value, Error> {
    let name = t.key()?;
    let enabled = t.flag("enabled")?;
    pmt_skills::activation::set_disabled(
        &*pmt_i18n::Strings::english(pmt_i18n::Surface::Core),
        t.scope.profile(),
        name,
        None,
        !enabled,
    )?;
    crate::plugins_host::skill_changed(&t.scope, name, if enabled { "enable" } else { "disable" });
    Ok(json!({"name": name, "enabled": enabled, "applies": "next_session"}))
}

fn skill_archive(t: &Target<'_>) -> Result<Value, Error> {
    let name = t.key()?;
    let curator = crate::curator_cmd::ops::Curator::open(t.scope.clone(), t.launch.clone())?;
    let said = curator.archive(name)?;
    crate::plugins_host::skill_changed(&t.scope, name, "archive");
    Ok(json!({"name": name, "archived": true, "message": said, "applies": "next_session"}))
}

fn toolset(t: &Target<'_>) -> Result<Value, Error> {
    let name = t.key()?;
    let enabled = t.flag("enabled")?;
    crate::tools_cmd::configure::persist_toolset(&t.scope, name, enabled)?;
    Ok(json!({"name": name, "enabled": enabled, "applies": "next_session"}))
}

fn cron_create(t: &Target<'_>) -> Result<Value, Error> {
    let now = Timestamp::now();
    let scripts = scripts_dir(&t.scope);
    let catalog = HostCatalog::new(&t.scope, t.launch);
    let profile = ops::Profile {
        store: &t.store,
        scripts_dir: &scripts,
        zone: profile_zone(&t.scope)?,
        catalog: &catalog,
    };
    let fields = crate::cron_host::tool::fields(&t.body, &t.scope)?;
    let spec = fields.new_spec(now, &profile.zone)?;
    let new = ops::NewJob {
        name: t.body["name"].as_str().map(str::to_owned),
        origin: None,
        paused: t.body["paused"]
            .as_bool()
            .unwrap_or(false)
            .then(String::new),
    };
    let job = ops::create(&profile, spec, new, now)?;
    t.profile.cron_wake.notify_waiters();
    Ok(crate::cron_host::tool::summary(&job))
}

fn cron_update(t: &Target<'_>) -> Result<Value, Error> {
    let now = Timestamp::now();
    let key = t.key()?;
    let scripts = scripts_dir(&t.scope);
    let catalog = HostCatalog::new(&t.scope, t.launch);
    let profile = ops::Profile {
        store: &t.store,
        scripts_dir: &scripts,
        zone: profile_zone(&t.scope)?,
        catalog: &catalog,
    };
    let fields = crate::cron_host::tool::fields(&t.body, &t.scope)?;
    let name = t.body["name"].as_str();
    let (mut job, _) = ops::update(
        &profile,
        key,
        name,
        |spec| fields.apply(spec, now, &profile.zone),
        now,
    )?;
    match t.body["enabled"].as_bool() {
        Some(false) => {
            job = ops::pause(&t.store, job.id(), Some("paused from the dashboard"), now)?;
        }
        Some(true) => job = ops::resume(&t.store, job.id(), now, &profile.zone)?,
        None => {}
    }
    t.profile.cron_wake.notify_waiters();
    Ok(crate::cron_host::tool::summary(&job))
}

fn cron_delete(t: &Target<'_>) -> Result<Value, Error> {
    let job = ops::remove(&t.store, t.key()?)?;
    Ok(json!({"removed": crate::cron_host::tool::summary(&job)}))
}

fn cron_run(t: &Target<'_>) -> Result<Value, Error> {
    let (job, run) = ops::manual_run(&t.store, t.key()?, Timestamp::now())?;
    t.profile.cron_wake.notify_waiters();
    Ok(
        json!({"job": crate::cron_host::tool::summary(&job), "run_id": run.id, "status": run.status}),
    )
}

fn memory(t: &Target<'_>) -> Result<Value, Error> {
    crate::memory_cmd::dashboard_patch(&t.scope, &t.store, &t.body)
}

fn text_list(value: &Value) -> Vec<String> {
    match value {
        Value::String(text) => text
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

fn webhook_create(t: &Target<'_>) -> Result<Value, Error> {
    let body = &t.body;
    let name = body["name"]
        .as_str()
        .ok_or_else(|| Error::new(&codes::CLI_USAGE, "a webhook subscription needs a name"))?;
    let text = |key: &str| {
        body[key]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let args = crate::webhooks_cmd::SubscribeArgs {
        name: name.to_owned(),
        events: text_list(&body["events"]),
        skills: text_list(&body["skills"]),
        deliver: text("deliver"),
        prompt: text("prompt"),
        secret_name: None,
        deliver_only: body["deliver_only"].as_bool().unwrap_or(false),
        script: None,
        cron_job: None,
        mirror_to_session: false,
    };
    let description = body["description"].as_str().unwrap_or_default();
    let created = crate::webhooks_cmd::create(&t.scope, &args, description)?;
    Ok(
        json!({"name": name, "url": created.url, "secret_name": created.secret_name,
              "secret": created.secret}),
    )
}

fn webhook_update(t: &Target<'_>) -> Result<Value, Error> {
    let name = t.key()?;
    let enabled = t.flag("enabled")?;
    crate::webhooks_cmd::set_enabled(&t.scope, name, enabled)?;
    Ok(json!({"name": name, "enabled": enabled}))
}

fn webhook_delete(t: &Target<'_>) -> Result<Value, Error> {
    let name = t.key()?;
    crate::webhooks_cmd::delete(&t.scope, name)?;
    Ok(json!({"name": name, "deleted": true}))
}

/// `prune-sessions` (older than `days`, default `sessions.retention_secs`
/// or 90 days), `vacuum`, `rebuild-fts`, `prune-snapshots`.
fn maintenance(t: &Target<'_>) -> Result<Value, Error> {
    let action = t.key()?;
    match action {
        "prune-sessions" => {
            let configured = t.scope.config().sessions.retention_secs;
            let older_than = match t.body["days"].as_u64() {
                Some(days) => Duration::from_hours(24 * days),
                None if configured > 0 => Duration::from_secs(configured),
                None => Duration::from_hours(24 * 90),
            };
            let pruned = t.store.prune(older_than, false)?;
            let ids: Vec<String> = pruned.iter().map(ToString::to_string).collect();
            crate::plugins_host::sessions_deleted(&t.scope, &ids);
            Ok(json!({"action": action, "deleted": ids.len(), "sessions": ids}))
        }
        "vacuum" => {
            t.store.vacuum()?;
            Ok(json!({"action": action, "done": true}))
        }
        "rebuild-fts" => {
            let indexed = t.store.repair_indexes()?;
            Ok(json!({"action": action, "indexed_messages": indexed}))
        }
        "prune-snapshots" => {
            use pmt_tools::snapshots::manage::{PrunePolicy, prune};
            let store = pmt_tools::snapshots::SnapshotStore::of_profile(t.scope.dir());
            let policy = PrunePolicy {
                retention: Duration::from_hours(24 * t.body["days"].as_u64().unwrap_or(7)),
                max_bytes: t.scope.config().files.snapshot.max_bytes,
                orphans: true,
            };
            let pruned = prune(&store, policy)
                .map_err(|e| Error::new(&codes::HOME_IO, format!("snapshot prune: {e}")))?;
            Ok(
                json!({"action": action, "workspaces": pruned.workspaces.len(),
                      "checkpoints": pruned.checkpoints, "bytes": pruned.bytes}),
            )
        }
        other => Err(Error::new(
            &codes::CLI_USAGE,
            format!("unknown maintenance action {other}"),
        )),
    }
}
