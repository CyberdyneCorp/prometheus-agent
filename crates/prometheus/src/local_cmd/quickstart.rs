//! The `Local models` entry of `prometheus model` (spec: local-models,
//! Local models command): show the hardware-priced catalog with the
//! recommendation marked, install the pinned engine, download the chosen
//! model with live progress (Ctrl+C pauses; choosing it again resumes the
//! same job), let sessions start the server (`local_runtime.enabled`), and
//! hand the model back so the picker makes it the profile's default
//! (validating the route starts the server).

use std::io::{BufRead, Write};

use pmt_core::error::{Error, codes};
use pmt_local::catalog::{self, Fit, Priced};
use pmt_local::engine::Outcome;
use pmt_local::pull::{self, Pulled};

use super::{Local, interruptible, size};
use crate::context::Context;

/// Runs the quickstart; the chosen model's library id, or `None` when the
/// user left or paused a download.
///
/// # Errors
///
/// The install, download or config errors.
pub(crate) fn run(
    ctx: &Context<'_>,
    input: &mut impl BufRead,
    out: &mut impl Write,
) -> Result<Option<String>, Error> {
    let local = Local::open(ctx)?;
    let Some(chosen) = choose(&local, input, out)? else {
        return Ok(None);
    };
    if !install_engine(&local, out)? || !download(&local, &chosen, out)? {
        return Ok(None);
    }
    if !local.config.enabled {
        pmt_config::daemon_local::set_enabled(&local.root, true)?;
        say(
            out,
            "set local_runtime.enabled = true: sessions start the server when they need it",
        )?;
    }
    Ok(Some(chosen.id))
}

/// The priced catalog, numbered, with the recommendation marked; the
/// default answer is the recommendation.
fn choose(
    local: &Local,
    input: &mut impl BufRead,
    out: &mut impl Write,
) -> Result<Option<Priced>, Error> {
    let hardware = local.hardware();
    let selection = local.selection(&hardware);
    let rows = catalog::price(&local.catalog(), &selection);
    let items: Vec<String> = rows.iter().map(describe).collect();
    say(
        out,
        &format!(
            "local models for this machine ({} backend):",
            selection.backend
        ),
    )?;
    if !rows.iter().any(|r| r.recommended) {
        say(
            out,
            "no model reaches 20 tokens/s entirely in GPU memory here",
        )?;
    }
    let picked = crate::model_cmd::picker::ask(input, out, "model", &items)?;
    Ok(picked.map(|i| rows[i].clone()))
}

fn describe(row: &Priced) -> String {
    let fit = match row.fit {
        Fit::FitsGpu => "fits GPU",
        Fit::UsesSystemRam => "uses system RAM",
        Fit::TooBig => "too big",
    };
    let mark = if row.recommended {
        "  (recommended)"
    } else {
        ""
    };
    format!(
        "{} {} {fit}, {}, ~{} tok/s{mark}",
        row.id,
        row.quant,
        size(row.download_bytes),
        row.tokens_per_second
    )
}

/// Installs the engine unless it is; false when paused.
fn install_engine(local: &Local, out: &mut impl Write) -> Result<bool, Error> {
    let hardware = local.hardware();
    let backend = local.selection(&hardware).backend;
    let engine = local.engine();
    if engine.installed(backend).is_some() {
        return Ok(true);
    }
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
        Outcome::Installed(_) => Ok(true),
        Outcome::Paused(job) => {
            say(
                out,
                &format!("engine download paused (job {job}); choose Local models again to resume"),
            )?;
            Ok(false)
        }
    }
}

/// Downloads the model unless it is in the library; false when paused.
fn download(local: &Local, chosen: &Priced, out: &mut impl Write) -> Result<bool, Error> {
    let library = local.library();
    let jobs = local.jobs();
    if library.model(&chosen.id, &jobs).is_ok() {
        return Ok(true);
    }
    let catalog = local.catalog();
    let entry = catalog.entry(&chosen.id)?;
    let mut job = pull::catalog_job(&catalog, entry, chosen, &library, &jobs)?;
    let downloader = local.downloader()?;
    let pulled = interruptible(&chosen.id, |control, mut printer| async move {
        pull::run(&library, &jobs, &downloader, &mut job, &control, &mut |p| {
            printer.show(p);
        })
        .await
    })?;
    match pulled {
        Pulled::Ready(_) => Ok(true),
        Pulled::Paused(id) => {
            say(
                out,
                &format!("download paused (job {id}); choose the model again to resume"),
            )?;
            Ok(false)
        }
    }
}

fn say(out: &mut impl Write, line: &str) -> Result<(), Error> {
    writeln!(out, "{line}")
        .map_err(|e| Error::new(&codes::OUTPUT_IO, format!("the model picker failed: {e}")))
}
