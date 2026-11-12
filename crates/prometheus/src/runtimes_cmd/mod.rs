//! `prometheus runtimes` (spec: distribution-and-update, Runtime CLI;
//! Managed runtime store; Runtime leases and garbage collection; Persistent
//! runtime opt-outs): `list`, `install`, `remove`, `verify`, `gc`, `doctor`,
//! `exclude list|add|remove`, and the hidden `refresh` the updater runs
//! with the new binary. The store itself is `pmt_update::runtimes`.

pub mod args;
pub mod browser;
mod bundle;
pub mod checks;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use pmt_config::daemon::{self, DaemonConfig};
use pmt_core::error::Error;
use pmt_update::runtimes::gc::{self, GcReport};
use pmt_update::runtimes::install::Installer;
use pmt_update::runtimes::lazy::human_size;
use pmt_update::runtimes::{Listed, Store};
use serde_json::json;

use crate::cli::{ExcludeCommand, RuntimesCommand};
use crate::context::Context;
use crate::model_cmd::block_on;
use crate::output::{document, print_raw};

/// Runs one `runtimes` subcommand.
///
/// # Errors
///
/// The command's failure (unknown name 2, excluded 1, download 69,
/// digest mismatch 77, I/O 74).
pub fn run(command: &RuntimesCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let home = ctx.home()?;
    let root = home.root();
    let config = DaemonConfig::load(root)?;
    let store = Store::open(root);
    match command {
        RuntimesCommand::List => list(&store, &config, ctx.json),
        RuntimesCommand::Install {
            names,
            from_bundle: Some(file),
        } => bundle::install(ctx, &store, &config, names, file),
        RuntimesCommand::Install {
            names,
            from_bundle: None,
        } => install(ctx, &store, &config, names),
        RuntimesCommand::Remove { name } => remove(&store, name),
        RuntimesCommand::Verify => verify(&store, ctx.json),
        RuntimesCommand::Gc => {
            let report = collect(&store, None)?;
            print_gc(&report);
            Ok(0)
        }
        RuntimesCommand::Doctor => checks::print_runtimes(root, ctx.json),
        RuntimesCommand::Exclude { command } => exclude(root, &config, command, ctx.json),
        RuntimesCommand::Refresh { bundle_dir } => {
            refresh(ctx, &store, &config, bundle_dir.as_deref())
        }
    }
}

/// An installer for `store` with the home's sources and network settings;
/// progress goes to stderr.
#[must_use]
pub fn installer(ctx: &Context<'_>, store: Store, config: &DaemonConfig) -> Installer {
    let progress: pmt_update::runtimes::install::Progress = Arc::new(|line| eprintln!("{line}"));
    browser::installer(
        store,
        config,
        &crate::update_cmd::outbound_policy(ctx),
        progress,
    )
}

fn list(store: &Store, config: &DaemonConfig, json: bool) -> Result<u8, Error> {
    let rows: Vec<Listed> = store
        .list()
        .into_iter()
        .map(|mut row| {
            row.excluded = config.runtimes.exclude.contains(&row.name);
            row
        })
        .collect();
    if json {
        return document(
            "runtimes list",
            &json!({ "platform": store.platform(), "runtimes": rows }),
        );
    }
    let mut out = String::new();
    for row in &rows {
        let excluded = if row.excluded { " (excluded)" } else { "" };
        let _ = writeln!(out, "{} {} pinned{excluded}", row.name, row.pinned);
        if row.installed.is_empty() {
            out.push_str("  not installed\n");
        }
        for v in &row.installed {
            let pinned = if v.pinned { "pinned" } else { "unpinned" };
            let _ = writeln!(
                out,
                "  {} {pinned} {} {} user(s)",
                v.dir,
                human_size(v.size),
                v.users
            );
        }
    }
    print_raw(&out)
}

fn install(
    ctx: &Context<'_>,
    store: &Store,
    config: &DaemonConfig,
    names: &[String],
) -> Result<u8, Error> {
    // Every name is checked before anything is downloaded.
    for name in names {
        if let Some(Err(err)) = python_version(name) {
            return Err(err);
        }
        let pinned = python_version(name).map_or(name.as_str(), |_| "uv");
        store.index().pin(pinned)?;
        for runtime in [
            pinned,
            if python_version(name).is_some() {
                "cpython"
            } else {
                pinned
            },
        ] {
            if config.runtimes.exclude.iter().any(|e| e == runtime) {
                return Err(pmt_update::runtimes::install::excluded(runtime));
            }
        }
    }
    let installer = installer(ctx, store.clone(), config);
    for name in names {
        if let Some(version) = python_version(name) {
            install_python(&installer, store, version?)?;
            continue;
        }
        let runtime = block_on(installer.install(name))??;
        println!(
            "{name} {} installed at {}",
            runtime.record.version,
            runtime.dir.display()
        );
    }
    Ok(0)
}

/// Installs the python bridge's interpreter `version` (and uv) when it is
/// missing, under the lazy installation policy: not when
/// `runtimes.allow_lazy_install = false` or `runtimes.exclude` names
/// `cpython` or `uv` (spec: python-bridge, Managed CPython runtime; used by
/// `plugins install`/`update`, which may use the network).
///
/// # Errors
///
/// The policy refusal or the download failure.
pub fn ensure_python(
    ctx: &Context<'_>,
    version: pmt_config::schema::PythonVersion,
) -> Result<(), Error> {
    let home = ctx.home()?;
    let root = home.root();
    if pmt_pybridge::runtime::find(root, version).is_some()
        && pmt_pybridge::runtime::uv(root).is_some()
    {
        return Ok(());
    }
    let config = DaemonConfig::load(root)?;
    if !config.runtimes.allow_lazy_install {
        return Err(pmt_pybridge::runtime::missing(version));
    }
    for runtime in ["uv", "cpython"] {
        if config.runtimes.exclude.iter().any(|e| e == runtime) {
            return Err(pmt_update::runtimes::install::excluded(runtime));
        }
    }
    let store = Store::open(root);
    let installer = installer(ctx, store.clone(), &config);
    install_python(&installer, &store, version)
}

/// `python@<version>`: the managed CPython of the python bridge.
fn python_version(name: &str) -> Option<Result<pmt_config::schema::PythonVersion, Error>> {
    let version = name.strip_prefix("python@")?;
    Some(
        pmt_config::schema::PythonVersion::parse(version).ok_or_else(|| {
            Error::new(
                &pmt_core::error::codes::RUNTIME_UNKNOWN,
                format!("unknown Python version '{version}' (expected 3.11, 3.12 or 3.13)"),
            )
        }),
    )
}

/// Installs the pinned uv, then CPython `version` with it (spec:
/// python-bridge, Managed CPython runtime).
fn install_python(
    installer: &Installer,
    store: &Store,
    version: pmt_config::schema::PythonVersion,
) -> Result<(), Error> {
    let uv = block_on(installer.install("uv"))??;
    let home = store.root().parent().unwrap_or(store.root());
    let python =
        pmt_pybridge::runtime::install(&pmt_pybridge::runtime::Uv::new(uv.exe(), home), version)?;
    println!(
        "python@{version} {} installed at {}",
        python.version,
        python.dir.display()
    );
    Ok(())
}

fn remove(store: &Store, name: &str) -> Result<u8, Error> {
    let (removed, kept) = store.remove(name)?;
    for dir in &removed {
        println!("removed {}", dir.display());
    }
    for dir in &kept {
        println!("kept {} (in use)", dir.display());
    }
    if removed.is_empty() && kept.is_empty() {
        println!("{name} is not installed");
    }
    Ok(0)
}

fn verify(store: &Store, json: bool) -> Result<u8, Error> {
    let results = store.verify();
    let failed = results.iter().any(|r| r.mismatch.is_some());
    if json {
        document(
            "runtimes verify",
            &json!({ "runtimes": results, "failed": failed }),
        )?;
    } else {
        let mut out = String::new();
        if results.is_empty() {
            out.push_str("no managed runtimes installed\n");
        }
        for r in &results {
            match &r.mismatch {
                None => {
                    let _ = writeln!(out, "✓ {} {}", r.name, r.dir);
                }
                Some(path) => {
                    let _ = writeln!(
                        out,
                        "✗ {}: realized bytes do not match recorded digest ({} in {})",
                        r.name, path, r.dir
                    );
                }
            }
        }
        print_raw(&out)?;
    }
    Ok(if failed {
        pmt_core::exit::FAILURE
    } else {
        pmt_core::exit::SUCCESS
    })
}

/// Runs `gc`, keeping this binary's pins and those of the other installed
/// binary: `other` when given (the updater's), else `prometheus.prev`
/// beside this executable.
///
/// # Errors
///
/// `runtime.io`.
pub fn collect(store: &Store, other: Option<&Path>) -> Result<GcReport, Error> {
    let mut pinned: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut keep = |pins: BTreeMap<String, String>| {
        for (name, dir) in pins {
            pinned.entry(name).or_default().insert(dir);
        }
    };
    keep(store.pins());
    let prev = std::env::current_exe()
        .ok()
        .map(|exe| pmt_update::swap::prev_path(&exe));
    if let Some(binary) = other.map(Path::to_owned).or(prev).filter(|p| p.is_file()) {
        match pmt_update::probe::version(&binary) {
            Ok(info) => keep(info.runtime_pins),
            Err(err) => tracing::warn!("runtimes gc: {err}"),
        }
    }
    gc::collect(store, &pinned, std::time::SystemTime::now())
}

fn print_gc(report: &GcReport) {
    for dir in &report.removed {
        println!("removed {}", dir.display());
    }
    for dir in &report.leased {
        println!("kept {} (in use)", dir.display());
    }
    println!("reclaimed {} bytes", report.reclaimed);
}

fn exclude(
    root: &Path,
    config: &DaemonConfig,
    command: &ExcludeCommand,
    json: bool,
) -> Result<u8, Error> {
    let mut names = config.runtimes.exclude.clone();
    match command {
        ExcludeCommand::List => {
            if json {
                return document("runtimes exclude list", &json!({ "exclude": names }));
            }
            let mut out = names.join("\n");
            if !out.is_empty() {
                out.push('\n');
            }
            return print_raw(&out);
        }
        ExcludeCommand::Add { name } => {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        ExcludeCommand::Remove { name } => names.retain(|n| n != name),
    }
    daemon::set_runtime_exclude(root, &names)?;
    println!("runtimes.exclude = [{}]", names.join(", "));
    Ok(0)
}

/// The updater's post-swap step, run by the new binary: install this
/// build's pin of every runtime that has some version installed (never an
/// excluded one), then collect what neither build pins.
fn refresh(
    ctx: &Context<'_>,
    store: &Store,
    config: &DaemonConfig,
    bundle_dir: Option<&Path>,
) -> Result<u8, Error> {
    let in_use: Vec<String> = store
        .index()
        .names()
        .into_iter()
        .filter(|name| !config.runtimes.exclude.iter().any(|n| n == name))
        .filter(|name| !store.installed_versions(name).is_empty())
        .filter(|name| store.installed(name).is_none())
        .map(str::to_owned)
        .collect();
    if !in_use.is_empty() {
        let mut installer = installer(ctx, store.clone(), config);
        if let Some(dir) = bundle_dir {
            installer = installer.with_bundle(dir);
        }
        for name in &in_use {
            block_on(installer.install(name))??;
            eprintln!("refreshed {name}");
        }
    }
    let report = collect(store, None)?;
    eprintln!("runtimes gc reclaimed {} bytes", report.reclaimed);
    Ok(0)
}
