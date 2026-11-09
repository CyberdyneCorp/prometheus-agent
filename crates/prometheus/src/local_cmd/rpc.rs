//! The `local.*` RPC methods, served by the daemon (spec: local-models,
//! Local models command), and the `--json` documents they share with the
//! command line. Downloads run as background tasks of the daemon and are
//! followed with `local.jobs`; a server `local.start` starts is supervised
//! by the daemon and stops with it.

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_gateway::local::LocalRequest;
use pmt_local::catalog;
use pmt_local::download::{Control, Job};
use pmt_local::hub::Hub;
use pmt_local::pull;
use pmt_rpc::contracts::{LocalParams, LocalReply};
use serde_json::{Value, json};

use super::Local;

/// `local status` document.
pub(super) async fn status_doc(local: &Local) -> Result<Value, Error> {
    let status = pmt_local::status::status(&local.root, local.hardware()).await?;
    let models = local.library().list(&local.jobs());
    Ok(json!({ "status": status, "models": models }))
}

/// `local catalog` document.
pub(super) fn catalog_doc(local: &Local) -> Value {
    let hardware = local.hardware();
    let selection = local.selection(&hardware);
    let rows = catalog::price(&local.catalog(), &selection);
    json!({
        "backend": selection.backend,
        "gpu": selection.gpu.as_ref().map(|g| &g.name),
        "gpu_budget_bytes": selection.gpu_budget,
        "recommended": rows.iter().find(|r| r.recommended).map(|r| &r.id),
        "models": rows,
    })
}

/// `local jobs` document.
pub(super) fn jobs_doc(local: &Local) -> Value {
    json!({ "jobs": local.jobs().views() })
}

/// `local search` document.
pub(super) async fn search_doc(local: &Local, query: &str) -> Result<Value, Error> {
    let hardware = local.hardware();
    let selection = local.selection(&hardware);
    let hub = Hub::new(&local.hub_url(), local.client()?);
    let found = hub.search(query, &selection).await?;
    Ok(json!({ "query": query, "results": found }))
}

/// The job pulling `model` (catalog id or `owner/repo[:quant]`).
pub(super) async fn pull_job(local: &Local, model: &str) -> Result<Job, Error> {
    let jobs = local.jobs();
    let library = local.library();
    let hardware = local.hardware();
    let selection = local.selection(&hardware);
    if model.contains('/') {
        let hub = Hub::new(&local.hub_url(), local.client()?);
        return hub.job(model, &selection, &library, &jobs).await;
    }
    let catalog = local.catalog();
    let entry = catalog.entry(model)?;
    let priced = catalog::price(&catalog, &selection)
        .into_iter()
        .find(|r| r.id == entry.id)
        .ok_or_else(|| {
            Error::new(
                &codes::LOCAL_NOT_FOUND,
                format!("'{model}' has no build for this machine"),
            )
        })?;
    pull::catalog_job(&catalog, entry, &priced, &library, &jobs)
}

/// Serves one `local.*` call for `scope`'s profile.
///
/// # Errors
///
/// The matching command's errors; `cli.usage` for a missing parameter.
pub async fn handle(scope: &ProfileScope, request: LocalRequest) -> Result<LocalReply, Error> {
    let local = Local::for_home(scope.home(), policy(scope))?;
    let params = request.params;
    match request.op {
        "status" => reply("local models status", status_doc(&local).await?),
        "catalog" => reply("the catalog priced for this machine", catalog_doc(&local)),
        "jobs" => reply("download jobs", jobs_doc(&local)),
        "search" => {
            let query = need(params.query.as_deref(), "query")?;
            reply(
                &format!("models matching '{query}'"),
                search_doc(&local, query).await?,
            )
        }
        "install" | "update" => install_in_background(local, request.op == "update"),
        "pull" => {
            let job = pull_job(&local, need(params.model.as_deref(), "model")?).await?;
            run_in_background(local, job)
        }
        "resume" => {
            let job = local.jobs().load(need(params.job.as_deref(), "job")?)?;
            if job.subject.starts_with("engine ") {
                return install_in_background(local, false);
            }
            run_in_background(local, job)
        }
        other => edit(&local, scope, other, &params).await,
    }
}

/// The calls that change the library, the profile or the server.
async fn edit(
    local: &Local,
    scope: &ProfileScope,
    op: &str,
    params: &LocalParams,
) -> Result<LocalReply, Error> {
    match op {
        "pause" => {
            let view = local.jobs().pause(need(params.job.as_deref(), "job")?)?;
            reply(&format!("job {} {}", view.id, view.state), json!(view))
        }
        "add" => {
            let path = need(params.path.as_deref(), "path")?;
            let model = local.library().link(std::path::Path::new(path), |i| {
                pull::start_window(i.context_length)
            })?;
            pmt_local::server::refresh(&local.root).await?;
            reply(&format!("linked {path} as {}", model.id), json!(model))
        }
        "rm" => {
            let model = need(params.model.as_deref(), "model")?;
            local.library().remove(model, &local.jobs())?;
            pmt_local::server::refresh(&local.root).await?;
            reply(&format!("removed {model}"), Value::Null)
        }
        "use" => {
            let model = local
                .library()
                .model(need(params.model.as_deref(), "model")?, &local.jobs())?;
            scope.profile().config_file().set_all(&[
                ("model.provider", pmt_providers::local_runtime::PROVIDER_ID),
                ("model.default", model.id.as_str()),
            ])?;
            reply(
                &format!("profile {} now uses llamacpp/{}", scope.name(), model.id),
                Value::Null,
            )
        }
        "start" => {
            let endpoint = pmt_local::server::Runtime::new(
                &local.root,
                Some(super::runtime::helper(&local.root)),
            )
            .start()
            .await?;
            reply(
                &format!("the managed llama-server runs at {}", endpoint.base_url),
                json!(endpoint),
            )
        }
        "stop" => match pmt_local::server::supervise::stop_server(&local.root) {
            Some(r) => reply(
                &format!("stopped the managed llama-server (pid {})", r.pid),
                Value::Null,
            ),
            None => reply("no managed llama-server is running", Value::Null),
        },
        other => Err(Error::new(
            &codes::CLI_USAGE,
            format!("unknown local method local.{other}"),
        )),
    }
}

fn install_in_background(local: Local, update: bool) -> Result<LocalReply, Error> {
    let hardware = local.hardware();
    let backend = local.selection(&hardware).backend;
    let engine = local.engine();
    engine.index().build(&hardware.platform, backend)?;
    let version = engine.index().version.clone();
    let downloader = local.downloader()?;
    tokio::spawn(async move {
        let jobs = local.jobs();
        let done = engine
            .install(
                &hardware.platform,
                backend,
                &downloader,
                &jobs,
                &Control::default(),
                &mut |_| {},
            )
            .await;
        match done {
            Ok(pmt_local::engine::Outcome::Installed(installed)) if update => {
                let in_use = pmt_local::server::record::live(&local.root).map(|r| r.engine);
                let _ = engine.remove_others(&installed.dir, in_use.as_deref());
            }
            Ok(_) => {}
            Err(err) => tracing::warn!(component = "local", "engine install failed: {err}"),
        }
    });
    reply(
        &format!("installing llama.cpp {version} ({backend}); follow it with local.jobs"),
        Value::Null,
    )
}

fn run_in_background(local: Local, mut job: Job) -> Result<LocalReply, Error> {
    let view = local.jobs().view(&job);
    let downloader = local.downloader()?;
    tokio::spawn(async move {
        let (library, jobs) = (local.library(), local.jobs());
        match pull::run(
            &library,
            &jobs,
            &downloader,
            &mut job,
            &Control::default(),
            &mut |_| {},
        )
        .await
        {
            Ok(pull::Pulled::Ready(_)) => {
                let _ = pmt_local::server::refresh(&local.root).await;
            }
            Ok(pull::Pulled::Paused(_)) => {}
            Err(err) => tracing::warn!(component = "local", "download {} failed: {err}", job.id),
        }
    });
    reply(
        &format!(
            "downloading {} as job {}; follow it with local.jobs",
            view.subject, view.id
        ),
        json!(view),
    )
}

#[allow(clippy::unnecessary_wraps)] // the answer of every arm of `handle`
fn reply(message: &str, data: Value) -> Result<LocalReply, Error> {
    Ok(LocalReply {
        message: message.to_owned(),
        data,
    })
}

fn need<'a>(value: Option<&'a str>, name: &str) -> Result<&'a str, Error> {
    value.ok_or_else(|| {
        Error::new(
            &codes::CLI_USAGE,
            format!("this local method needs '{name}'"),
        )
    })
}

fn policy(scope: &ProfileScope) -> pmt_net::OutboundPolicy {
    let network = &scope.config().network;
    pmt_net::OutboundPolicy {
        force_ipv4: network.force_ipv4,
        extra_ca_file: network.extra_ca_file.clone().map(Into::into),
        proxy: network.proxy.clone(),
    }
}
