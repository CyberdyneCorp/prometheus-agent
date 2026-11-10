//! `prometheus local catalog|pull|pause|resume|jobs|search|add|rm|use`.

use std::fmt::Write as _;
use std::path::Path;

use pmt_core::error::Error;
use pmt_local::catalog::{self, Fit};
use pmt_local::download::Job;
use pmt_local::hub::Hub;
use pmt_local::pull::{self, Pulled};

use super::engine::paused;
use super::{Local, interruptible, size};
use crate::context::Context;
use crate::model_cmd::block_on;
use crate::output::{document, print, print_raw};

/// `local catalog [--json]`.
pub(super) fn catalog(local: &Local) -> Result<u8, Error> {
    if local.json {
        return document("local catalog", &super::rpc::catalog_doc(local));
    }
    let hardware = local.hardware();
    let rows = catalog::price(&local.catalog(), &local.selection(&hardware));
    let mut out = String::new();
    for row in &rows {
        let mark = if row.recommended {
            " (recommended)"
        } else {
            ""
        };
        let fit = match row.fit {
            Fit::FitsGpu => "fits GPU",
            Fit::UsesSystemRam => "uses system RAM",
            Fit::TooBig => "too big",
        };
        let _ = writeln!(
            out,
            "{}{mark}: {} {fit}, {}, window {}..{}, ~{} tok/s",
            row.id,
            row.quant,
            size(row.download_bytes),
            row.context_start,
            row.context_max,
            row.tokens_per_second
        );
        if let Some(reason) = &row.reason {
            let _ = writeln!(out, "  {reason}");
        }
    }
    if !rows.iter().any(|r| r.recommended) {
        out.push_str("no model reaches 20 tokens/s entirely in GPU memory on this machine\n");
    }
    print_raw(&out)
}

/// `local pull <model>`: a catalog id, or `owner/repo[:quant]` from
/// Hugging Face. Ctrl+C pauses the job.
pub(super) fn pull(local: &Local, model: &str) -> Result<u8, Error> {
    let job = block_on(super::rpc::pull_job(local, model))??;
    eprintln!(
        "job {}: {} ({})",
        job.id,
        job.subject,
        size(job.total_bytes())
    );
    run_model_job(local, job)
}

fn run_model_job(local: &Local, mut job: Job) -> Result<u8, Error> {
    let jobs = local.jobs();
    let library = local.library();
    let downloader = local.downloader()?;
    let label = job.subject.clone();
    let pulled = interruptible(&label, |control, mut printer| async move {
        pull::run(&library, &jobs, &downloader, &mut job, &control, &mut |p| {
            printer.show(p);
        })
        .await
    })?;
    match pulled {
        Pulled::Paused(id) => Ok(paused(&id)),
        Pulled::Ready(model) => {
            block_on(pmt_local::server::refresh(&local.root))??;
            print(&format!(
                "{} is in the library (window {}); use it with: prometheus local use {}",
                model.id, model.context, model.id
            ))
        }
    }
}

/// `local pause <job>`.
pub(super) fn pause(local: &Local, id: &str) -> Result<u8, Error> {
    let view = local.jobs().pause(id)?;
    print(&format!("job {} {}", view.id, view.state))
}

/// `local resume <job>`.
pub(super) fn resume(local: &Local, id: &str) -> Result<u8, Error> {
    let job = local.jobs().load(id)?;
    if job.subject.starts_with("engine ") {
        return super::engine::install(local);
    }
    run_model_job(local, job)
}

/// `local jobs [--json]`.
pub(super) fn jobs(local: &Local) -> Result<u8, Error> {
    if local.json {
        return document("local jobs", &super::rpc::jobs_doc(local));
    }
    let views = local.jobs().views();
    if views.is_empty() {
        return print(&"no download jobs");
    }
    let mut out = String::new();
    for v in &views {
        let percent = (v.done_bytes * 100)
            .checked_div(v.total_bytes)
            .unwrap_or(100);
        let _ = writeln!(
            out,
            "{} {} {} {percent}% ({} of {})",
            v.id,
            v.subject,
            v.state,
            size(v.done_bytes),
            size(v.total_bytes)
        );
    }
    print_raw(&out)
}

/// `local search <query> [--json]`.
pub(super) fn search(local: &Local, query: &str) -> Result<u8, Error> {
    if local.json {
        return document(
            "local search",
            &block_on(super::rpc::search_doc(local, query))??,
        );
    }
    let hardware = local.hardware();
    let selection = local.selection(&hardware);
    let hub = Hub::new(&local.hub_url(), local.client()?);
    let found = block_on(hub.search(query, &selection))??;
    let mut out = String::new();
    for repo in &found {
        let _ = writeln!(out, "{} ({} downloads, community)", repo.id, repo.downloads);
        for file in &repo.files {
            let _ = writeln!(out, "  {} {} {:?}", file.path, size(file.size), file.fit);
        }
    }
    if found.is_empty() {
        out.push_str("no GGUF models found\n");
    }
    print_raw(&out)
}

/// `local add <path>`: links the file; the window comes from its metadata.
pub(super) fn add(local: &Local, path: &Path) -> Result<u8, Error> {
    let model = local
        .library()
        .link(path, |info| pull::start_window(info.context_length))?;
    block_on(pmt_local::server::refresh(&local.root))??;
    print(&format!(
        "linked {} as {} (window {}); the file stays where it is",
        path.display(),
        model.id,
        model.context
    ))
}

/// `local rm <model>`.
pub(super) fn rm(local: &Local, model: &str) -> Result<u8, Error> {
    local.library().remove(model, &local.jobs())?;
    block_on(pmt_local::server::refresh(&local.root))??;
    print(&format!("removed {model} from the library"))
}

/// `local use <model>`: `model.provider = "llamacpp"`, `model.default`.
pub(super) fn use_model(local: &Local, ctx: &Context<'_>, model: &str) -> Result<u8, Error> {
    let found = local.library().model(model, &local.jobs())?;
    let scope = ctx.scope()?;
    scope.profile().config_file().set_all(&[
        ("model.provider", pmt_providers::local_runtime::PROVIDER_ID),
        ("model.default", found.id.as_str()),
    ])?;
    print(&format!(
        "profile {} now uses llamacpp/{}",
        scope.name(),
        found.id
    ))
}
