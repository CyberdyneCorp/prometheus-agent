//! `prometheus local status|install|update|start|stop`.

use std::fmt::Write as _;

use pmt_core::error::Error;
use pmt_local::engine::Outcome;
use pmt_local::server::{self, Stop, record};

use super::{Local, interruptible, size};
use crate::context::Context;
use crate::model_cmd::block_on;
use crate::output::{document, print, print_raw};

/// `local status [--json]`.
pub(super) fn status(local: &Local) -> Result<u8, Error> {
    if local.json {
        return document("local status", &block_on(super::rpc::status_doc(local))??);
    }
    let status = block_on(pmt_local::status::status(&local.root, local.hardware()))??;
    let models = local.library().list(&local.jobs());
    let mut out = String::new();
    let installed = if status.engine.installed.is_empty() {
        "not installed".to_owned()
    } else {
        status.engine.installed.join(", ")
    };
    let _ = writeln!(
        out,
        "engine: llama.cpp {} ({} backend); installed: {installed}",
        status.engine.pinned, status.engine.backend
    );
    let server = &status.server;
    if let (Some(source), Some(port)) = (server.source, server.port) {
        let _ = writeln!(out, "server: running ({source}) on 127.0.0.1:{port}");
    } else {
        let mode = if server.enabled {
            "started on demand"
        } else {
            "detection only"
        };
        let _ = writeln!(out, "server: stopped ({mode})");
    }
    let resident = if status.resident.is_empty() {
        "none".to_owned()
    } else {
        status.resident.join(", ")
    };
    let _ = writeln!(out, "resident: {resident}");
    for gpu in &status.hardware.gpus {
        let _ = writeln!(
            out,
            "gpu: {} {} used of {}",
            gpu.name,
            size(gpu.memory_used),
            size(gpu.memory_total)
        );
    }
    let hw = &status.hardware;
    let _ = writeln!(
        out,
        "ram: {} free of {}",
        size(hw.ram_available),
        size(hw.ram_total)
    );
    for model in &models {
        let _ = writeln!(
            out,
            "model: {} {} {}",
            model.id,
            model.state,
            size(model.size)
        );
    }
    print_raw(&out)
}

/// `local install`: the pinned engine for this machine's backend. The
/// (backend, platform) pair is checked before any request.
pub(super) fn install(local: &Local) -> Result<u8, Error> {
    let hardware = local.hardware();
    let backend = local.selection(&hardware).backend;
    let engine = local.engine();
    engine.index().build(&hardware.platform, backend)?;
    let downloader = local.downloader()?;
    let jobs = local.jobs();
    let label = format!("llama.cpp {} ({backend})", engine.index().version);
    let outcome = interruptible(&label, |control, mut printer| async move {
        engine
            .install(
                &hardware.platform,
                backend,
                &downloader,
                &jobs,
                &control,
                &mut |p| {
                    printer.show(p);
                },
            )
            .await
    })?;
    match outcome {
        Outcome::Installed(installed) => print(&format!(
            "installed llama.cpp {} ({backend}) in {}",
            installed.record.version,
            installed.dir.display()
        )),
        Outcome::Paused(job) => Ok(paused(&job)),
    }
}

/// `local update`: installs this build's pin, then removes every other
/// engine build the running server does not use.
pub(super) fn update(local: &Local) -> Result<u8, Error> {
    let code = install(local)?;
    if code != 0 {
        return Ok(code);
    }
    let hardware = local.hardware();
    let backend = local.selection(&hardware).backend;
    let engine = local.engine();
    let Some(current) = engine.installed(backend) else {
        return Ok(code);
    };
    let in_use = record::live(&local.root).map(|r| r.engine);
    for removed in engine.remove_others(&current.dir, in_use.as_deref())? {
        print(&format!("removed {}", removed.display()))?;
    }
    Ok(0)
}

/// What a paused download prints; Ctrl+C exits 130.
pub(super) fn paused(job: &str) -> u8 {
    eprintln!("paused job {job}; resume it with: prometheus local resume {job}");
    pmt_core::exit::INTERRUPTED
}

/// `local start`: the daemon supervises the server when it runs;
/// otherwise this process does, in the foreground, until Ctrl+C.
pub(super) fn start(local: &Local, ctx: &Context<'_>) -> Result<u8, Error> {
    if let Some(found) = server::recorded(&local.root) {
        return print(&format!(
            "the managed llama-server already runs on 127.0.0.1:{}",
            found.port
        ));
    }
    if let Some(reply) = super::runtime::start_in_daemon(ctx)? {
        return print(&reply);
    }
    let stop = Stop::default();
    let watcher = stop.clone();
    std::thread::spawn(move || {
        let _ = block_on(async {
            if tokio::signal::ctrl_c().await.is_ok() {
                watcher.raise();
            }
        });
    });
    server::serve(&local.root, &stop, &mut |r| {
        eprintln!(
            "llama-server runs on 127.0.0.1:{} (pid {}); Ctrl+C stops it",
            r.port, r.pid
        );
    })?;
    Ok(0)
}

/// `local stop`.
pub(super) fn stop(local: &Local) -> Result<u8, Error> {
    match server::supervise::stop_server(&local.root) {
        Some(stopped) => print(&format!(
            "stopped the managed llama-server (pid {}, port {})",
            stopped.pid, stopped.port
        )),
        None => print(&"no managed llama-server is running"),
    }
}
