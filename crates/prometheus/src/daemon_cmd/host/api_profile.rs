//! The profile-level API endpoints the daemon answers (spec: api-server,
//! Jobs REST API; Discovery and readiness endpoints): scheduled jobs with
//! the `cronjob` tool's fields, validation, confirmation and messages
//! (jobs created here record origin `api`), the skills and toolsets
//! catalogs, and bounded readiness checks that never show configuration
//! values, credentials, paths, commands or raw errors.

use std::collections::BTreeSet;

use jiff::Timestamp;
use pmt_api::{Catalog, JobsCall};
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, ErrorKind};
use pmt_cron::ops;
use pmt_cron::target::JobOrigin;
use pmt_tools::SessionSurface;
use serde_json::{Value, json};

use super::{DaemonHost, Profile};
use crate::cron_host::tool::{fields, needs_confirmation, summary};
use crate::cron_host::{HostCatalog, profile_zone, scripts_dir};

/// The HTTP status and body of a jobs call.
pub(super) fn jobs(launch: &LaunchEnv, profile: &Profile, call: &JobsCall) -> (u16, Value) {
    match jobs_call(launch, profile, call) {
        Ok(answer) => {
            if call.action.as_deref() == Some("run") {
                profile.cron_wake.notify_waiters();
            }
            answer
        }
        Err(err) => {
            let status = match err.kind() {
                ErrorKind::NotFound => 404,
                ErrorKind::InvalidInput | ErrorKind::Usage | ErrorKind::Config => 400,
                ErrorKind::Conflict => 409,
                _ => 500,
            };
            (
                status,
                json!({"error": {"message": err.message(), "code": err.code().code}}),
            )
        }
    }
}

fn jobs_call(
    launch: &LaunchEnv,
    profile: &Profile,
    call: &JobsCall,
) -> Result<(u16, Value), Error> {
    let store = &profile.store;
    let scope = profile.scope();
    let now = Timestamp::now();
    let job = call.job.as_deref();
    match (call.method.as_str(), job, call.action.as_deref()) {
        ("GET", None, None) => {
            let jobs: Vec<Value> = store
                .cron_jobs()?
                .into_iter()
                .filter_map(|row| pmt_cron::job::Job::from_row(row).ok())
                .map(|job| summary(&job))
                .collect();
            Ok((200, json!({"object": "list", "data": jobs})))
        }
        ("POST", None, None) => create(launch, profile, &call.body, now),
        ("GET", Some(id), None) => Ok((200, summary(&ops::find(store, id)?))),
        ("PATCH", Some(id), None) => update(launch, profile, id, &call.body, now),
        ("DELETE", Some(id), None) => {
            Ok((200, json!({"removed": summary(&ops::remove(store, id)?)})))
        }
        ("POST", Some(id), Some("pause")) => {
            let reason = call.body["paused_reason"].as_str();
            Ok((200, summary(&ops::pause(store, id, reason, now)?)))
        }
        ("POST", Some(id), Some("resume")) => Ok((
            200,
            summary(&ops::resume(store, id, now, &profile_zone(&scope)?)?),
        )),
        ("POST", Some(id), Some("run")) => {
            let how = ops::QueueRun {
                context: call.body["prompt"]
                    .as_str()
                    .filter(|p| !p.trim().is_empty())
                    .map(str::to_owned),
                exclusive: true,
                ..ops::QueueRun::default()
            };
            let (job, run) = ops::queue_run(store, id, now, &how)?;
            Ok((
                202,
                json!({"status": "queued", "run_id": run.id, "job": summary(&job)}),
            ))
        }
        _ => Err(Error::new(
            &pmt_core::error::codes::CLI_USAGE,
            format!("{} is not a jobs operation", call.method),
        )),
    }
}

fn create(
    launch: &LaunchEnv,
    profile: &Profile,
    body: &Value,
    now: Timestamp,
) -> Result<(u16, Value), Error> {
    let scope = profile.scope();
    let scripts = scripts_dir(&scope);
    let catalog = HostCatalog::new(&scope, launch);
    let ops_profile = ops::Profile {
        store: &profile.store,
        scripts_dir: &scripts,
        zone: profile_zone(&scope)?,
        catalog: &catalog,
    };
    let origin = JobOrigin::Api;
    let mut spec = fields(body, &scope)?.new_spec(now, &ops_profile.zone)?;
    if spec.deliver.is_empty() {
        spec.deliver = ops::default_deliver(Some(&origin));
    }
    let name = body["name"].as_str().map(str::to_owned);
    let echo = ops::check(&ops_profile, &spec, now)?;
    match body["confirm_token"].as_str() {
        Some(token) if ops::redeem(token, &spec, name.as_deref(), now) => {
            let paused = body["paused"].as_bool().unwrap_or(false).then(|| {
                body["paused_reason"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            });
            let new = ops::NewJob {
                name,
                origin: Some(&origin),
                paused,
            };
            let job = ops::create(&ops_profile, spec, new, now)?;
            Ok((
                201,
                json!({"status": "created", "job": summary(&job), "summary": echo.render()}),
            ))
        }
        Some(_) => Err(Error::new(
            &pmt_core::error::codes::CRON_CONFIRMATION,
            "the confirm_token is unknown, expired or for different job fields: ask again without it",
        )),
        None => Ok((
            200,
            needs_confirmation(&echo, &ops::confirm_token(&spec, name.as_deref(), now)),
        )),
    }
}

fn update(
    launch: &LaunchEnv,
    profile: &Profile,
    key: &str,
    body: &Value,
    now: Timestamp,
) -> Result<(u16, Value), Error> {
    let scope = profile.scope();
    let scripts = scripts_dir(&scope);
    let catalog = HostCatalog::new(&scope, launch);
    let ops_profile = ops::Profile {
        store: &profile.store,
        scripts_dir: &scripts,
        zone: profile_zone(&scope)?,
        catalog: &catalog,
    };
    let changes = fields(body, &scope)?;
    let name = body["name"].as_str();
    if changes.reschedules() {
        let mut spec = ops::find(&profile.store, key)?.spec;
        changes.apply(&mut spec, now, &ops_profile.zone)?;
        let echo = ops::check(&ops_profile, &spec, now)?;
        let confirmed = body["confirm_token"]
            .as_str()
            .is_some_and(|token| ops::redeem(token, &spec, name, now));
        if !confirmed {
            return Ok((
                200,
                needs_confirmation(&echo, &ops::confirm_token(&spec, name, now)),
            ));
        }
    }
    let (job, _) = ops::update(
        &ops_profile,
        key,
        name,
        |spec| changes.apply(spec, now, &ops_profile.zone),
        now,
    )?;
    Ok((200, json!({"status": "updated", "job": summary(&job)})))
}

/// `GET /v1/skills` or `GET /v1/toolsets`.
pub(super) fn catalog(host: &DaemonHost, profile: &Profile, what: Catalog) -> Result<Value, Error> {
    let scope = profile.scope();
    match what {
        Catalog::Skills => {
            let catalog = pmt_skills::eject::profile_catalog(
                &scope,
                None,
                host.launch.user_home().map(std::path::PathBuf::as_path),
                pmt_skills::bundled::Bundled::builtin(),
            );
            let disabled = &scope.config().skills.disabled;
            Ok(catalog
                .skills
                .iter()
                .filter(|s| !disabled.contains(&s.name))
                .map(|s| json!({"name": s.name, "description": s.description, "category": s.category}))
                .collect())
        }
        Catalog::Toolsets => toolsets(&scope),
        Catalog::Models => Ok(model_options(host, profile)),
    }
}

/// Every provider/model pair the profile is configured with (its main
/// model, fallback chain, API routes and model aliases), each with the
/// catalog's capability hints and per-million-token prices.
fn model_options(host: &DaemonHost, profile: &Profile) -> Value {
    let scope = profile.scope();
    let config = scope.config();
    let mut pairs: Vec<(String, String, String)> = Vec::new();
    let mut push = |provider: &str, model: &str, source: String| {
        if !pairs.iter().any(|(p, m, _)| p == provider && m == model) {
            pairs.push((provider.to_owned(), model.to_owned(), source));
        }
    };
    if let Some(provider) = &config.model.provider {
        push(
            provider,
            config.model.default.as_deref().unwrap_or_default(),
            "default".to_owned(),
        );
    }
    for entry in &config.fallback.chain {
        push(&entry.provider, &entry.model, "fallback".to_owned());
    }
    for (alias, route) in &config.api_server.model_routes {
        push(
            &route.provider,
            route.model.as_deref().unwrap_or_default(),
            format!("route:{alias}"),
        );
    }
    let catalog = host
        .factory(
            profile,
            DaemonHost::cwd_of(profile, None),
            &pmt_rpc::contracts::SessionSetup::default(),
        )
        .ok();
    let mut providers: Vec<(String, Vec<Value>)> = Vec::new();
    for (provider, model, source) in pairs {
        let mut entry = json!({"id": model, "source": source});
        if let Some(factory) = &catalog {
            let (meta, _) = factory.models.catalog().metadata(&provider, &model);
            entry["context_window"] = meta.context_window.into();
            entry["capabilities"] = json!({"tools": meta.supports_tools, "vision": meta.supports_vision,
                "video": meta.supports_video, "reasoning": meta.supports_reasoning});
            entry["pricing"] = meta.price.map_or(Value::Null, |p| {
                json!({"currency": "USD", "unit": "1M tokens", "input": p.input, "output": p.output,
                    "cache_read": p.cache_read, "cache_write": p.cache_write})
            });
        }
        match providers.iter_mut().find(|(id, _)| *id == provider) {
            Some((_, models)) => models.push(entry),
            None => providers.push((provider, vec![entry])),
        }
    }
    let providers: Vec<Value> = providers
        .into_iter()
        .map(|(id, models)| json!({"id": id, "models": models}))
        .collect();
    json!({"object": "model.options", "providers": providers})
}

fn toolsets(scope: &pmt_config::ProfileScope) -> Result<Value, Error> {
    let config = scope.config();
    let (registry, extensions) = crate::tools_cmd::profile_registry(scope)?;
    let surface = SessionSurface::Api;
    let selection =
        crate::chat::session::tools::selection(&registry, config, false, &extensions, &surface)?;
    let active: BTreeSet<&str> = registry
        .toolsets()
        .filter(|t| {
            registry.members(&t.name).is_ok_and(|members| {
                !members.is_empty() && members.iter().all(|m| selection.contains(m))
            })
        })
        .map(|t| t.name.as_str())
        .collect();
    let configured: BTreeSet<&str> = config
        .tools
        .enabled_toolsets
        .iter()
        .chain(config.api_server.toolsets.iter().flatten())
        .map(String::as_str)
        .collect();
    let rows = crate::tools_cmd::toolset_rows(&registry, &BTreeSet::new())?;
    Ok(rows
        .iter()
        .map(|row| {
            json!({"name": row.name, "label": row.name, "description": row.description,
                "enabled": active.contains(row.name.as_str()),
                "configured": configured.contains(row.name.as_str()), "tools": row.tools})
        })
        .collect())
}

/// Bounded readiness checks of the profile.
pub(super) async fn health(host: &DaemonHost, profile: &Profile) -> Value {
    let scope = profile.scope();
    let name = scope.name().to_string();
    let config_check = match profile
        .config
        .lock()
        .expect("config state poisoned")
        .error()
    {
        None => json!({"status": "ok"}),
        Some(_) => json!({"status": "failed", "code": "config_invalid"}),
    };
    let store_check = match profile.store.session_counts(Timestamp::now()) {
        Ok(_) => json!({"status": "ok"}),
        Err(err) => json!({"status": "failed", "code": err.code().code}),
    };
    let model_check = match model_ready(host, profile).await {
        Ok(()) => json!({"status": "ok"}),
        Err(err) => json!({"status": "failed", "code": err.code().code}),
    };
    let free = crate::chat::autoprune::free_space(scope.dir());
    let low = scope.config().sessions.low_disk_bytes;
    let disk_check = match free {
        Some(free) if free < low => {
            json!({"status": "degraded", "code": "low_disk", "free_mb": free / 1_048_576})
        }
        Some(free) => json!({"status": "ok", "free_mb": free / 1_048_576}),
        None => json!({"status": "unknown"}),
    };
    let adapters: Vec<_> = pmt_gateway::observability::component_reports()
        .into_iter()
        .filter(|c| {
            c.component.starts_with("adapter:") && c.profile.as_deref() == Some(name.as_str())
        })
        .collect();
    let down = adapters.iter().filter(|c| c.state != "ok").count();
    let adapter_check = json!({"status": if down == 0 { "ok" } else { "degraded" },
        "total": adapters.len(), "not_ok": down});
    json!({
        "config": config_check,
        "state_db": store_check,
        "model": model_check,
        "disk": disk_check,
        "adapters": adapter_check,
        "delegations": {"status": "ok", "active": pmt_gateway::observability::subagents()},
    })
}

/// The profile's configured model resolves with a credential.
async fn model_ready(host: &DaemonHost, profile: &Profile) -> Result<(), Error> {
    let cwd = DaemonHost::cwd_of(profile, None);
    let setup = pmt_rpc::contracts::SessionSetup::default();
    let factory = host.factory(profile, cwd, &setup)?;
    let target = factory.models.primary(&super::setup::flags(&setup)).await?;
    let route = factory
        .models
        .resolver()
        .resolve_target(&target, &mut pmt_providers::resolve::Trace::default())
        .await?;
    factory
        .models
        .port(&crate::chat::session::model::target_of(&route))
        .await?;
    Ok(())
}
