//! `prometheus computer-use install|status|doctor|permissions` (spec:
//! computer-use, Computer-use CLI and doctor; Platform prerequisites;
//! Driver runtime management and identity).

use std::ffi::OsString;
use std::fmt::Write as _;

use pmt_config::ProfileScope;
use pmt_config::daemon::DaemonConfig;
use pmt_config::schema::{CUA_BACKEND, ComputerUseConfig, ComputerUsePermission, Overlay};
use pmt_core::error::{Error, codes};
use pmt_plugins::computer_use::PluginDesktop;
use pmt_plugins::host::CallInfo;
use pmt_update::runtimes::Store;
use serde_json::json;

use crate::computer_use::binary::{self, INSTALL_HINT, RUNTIME, Source};
use crate::computer_use::{autostart, backend, doctor, launcher, platform};
use crate::computer_use_cmd::{ComputerUseCommand, PermissionsCommand};
use crate::context::Context;
use crate::model_cmd::block_on;
use crate::output::{document, print_raw};

/// Runs a driver subcommand (`screen` is `computer_use_cmd`'s).
///
/// # Errors
///
/// `computer_use.unavailable`, runtime install errors, `home.io`.
pub fn run(command: &ComputerUseCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let vars = |name: &str| ctx.launch.inherited(name).map(OsString::from);
    let env = launcher::driver_env(&scope, &vars);
    match command {
        ComputerUseCommand::Install { upgrade } => install(ctx, &scope, *upgrade),
        ComputerUseCommand::Status => status(ctx, &scope, &env),
        ComputerUseCommand::Doctor { include, skip } => {
            let outcome = block_on(doctor::run(&scope, &env, include, skip))?;
            report(ctx, &outcome)
        }
        ComputerUseCommand::Permissions { command } => permissions(ctx, &scope, &env, command),
        ComputerUseCommand::Screen { .. } => unreachable!("computer_use_cmd runs screen"),
    }
}

fn install(ctx: &Context<'_>, scope: &ProfileScope, upgrade: bool) -> Result<u8, Error> {
    if let Some(path) = &scope.config().computer_use.driver_path {
        eprintln!(
            "computer_use.driver_path is set: {} is used as it is (never installed or replaced)",
            path.display()
        );
        autostart(ctx, scope, path)?;
        return Ok(0);
    }
    let home = scope.home();
    let config = DaemonConfig::load(home)?;
    let store = Store::open(home);
    if upgrade
        && store.installed(RUNTIME).is_none()
        && !store.installed_versions(RUNTIME).is_empty()
    {
        // An older pin is installed: the pinned version goes next to it.
        eprintln!("upgrading {RUNTIME} to this build's pinned version");
    }
    let installer = crate::runtimes_cmd::installer(ctx, Store::open(home), &config);
    let fresh = block_on(installer.install(RUNTIME))??;
    if upgrade {
        let stale: Vec<_> = store
            .installed_versions(RUNTIME)
            .into_iter()
            .filter(|v| v.dir_name() != fresh.dir_name())
            .collect();
        eprintln!(
            "{} older version(s) kept until `prometheus runtimes gc`",
            stale.len()
        );
    }
    eprintln!("{RUNTIME} installed: {}", fresh.exe().display());
    autostart(ctx, scope, &fresh.exe())?;
    Ok(0)
}

/// `computer_use.autostart` on Windows outside an interactive session:
/// registers the per-logon task that runs `driver` in the user's session.
fn autostart(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    driver: &std::path::Path,
) -> Result<(), Error> {
    let config = &scope.config().computer_use;
    let mut facts =
        platform::Facts::of(&|name: &str| ctx.launch.inherited(name).map(OsString::from));
    if !autostart::needed(config, &facts) {
        return Ok(());
    }
    autostart::ensure(config, &mut facts, driver)
        .map_err(|why| Error::new(&codes::COMPUTER_USE_UNAVAILABLE, why))?;
    eprintln!("registered the per-logon driver task {}", autostart::TASK);
    Ok(())
}

fn status(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    env: &[(String, OsString)],
) -> Result<u8, Error> {
    let config = &scope.config().computer_use;
    if config.backend != CUA_BACKEND {
        return plugin_status(ctx, scope);
    }
    let daemon = DaemonConfig::load(scope.home())?;
    let facts = platform::Facts::of(&|name: &str| {
        env.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    });
    let located = binary::installed(scope.home(), config, &daemon);
    let (driver, version, problem) = match &located {
        Ok(Some(driver)) => match binary::checked(&driver.path, env) {
            Ok(manifest) => (Some(driver), Some(manifest.version), None),
            Err(why) => (Some(driver), None, Some(why)),
        },
        Ok(None) => (
            None,
            None,
            Some(format!(
                "{RUNTIME} is not installed (installed on first use): {INSTALL_HINT}"
            )),
        ),
        Err(why) => (None, None, Some(why.clone())),
    };
    let body = json!({
        "backend": config.backend,
        "driver": driver.map(|d| d.path.display().to_string()),
        "source": driver.map(|d| if d.source == Source::Configured { "driver_path" } else { "managed" }),
        "version": version,
        "ready": problem.is_none(),
        "problem": problem,
        "permission_mode": permission_mode(config),
        "overlay": platform::overlay(config, &facts),
        "platform": facts.os,
    });
    if ctx.json {
        return document("computer-use status", &body);
    }
    let mut out = String::new();
    let _ = writeln!(out, "backend: {}", config.backend);
    match (&body["driver"], &body["version"]) {
        (serde_json::Value::String(path), serde_json::Value::String(version)) => {
            let _ = writeln!(
                out,
                "driver: {RUNTIME} {version} ({path}, {})",
                body["source"].as_str().unwrap_or_default()
            );
        }
        _ => out.push_str("driver: not ready\n"),
    }
    if let Some(problem) = body["problem"].as_str() {
        let _ = writeln!(out, "problem: {problem}");
    }
    let _ = writeln!(
        out,
        "permission mode: {}",
        body["permission_mode"].as_str().unwrap_or_default()
    );
    let _ = writeln!(
        out,
        "cursor overlay: {}",
        if body["overlay"] == true { "on" } else { "off" }
    );
    print_raw(&out)?;
    Ok(u8::from(body["ready"] != true))
}

fn permission_mode(config: &ComputerUseConfig) -> &'static str {
    match config.permission_mode {
        ComputerUsePermission::Standard => "standard",
        ComputerUsePermission::Bounded => "bounded",
    }
}

/// The status of a backend an enabled plugin provides: its own readiness
/// check.
fn plugin_status(ctx: &Context<'_>, scope: &ProfileScope) -> Result<u8, Error> {
    let config = &scope.config().computer_use;
    let cwd = std::env::current_dir().unwrap_or_default();
    let plugins = crate::plugins_host::for_session(scope, &cwd);
    let found = backend::plugin_backend(&config.backend, &plugins, &CallInfo::default());
    let checked = found.as_ref().map(PluginDesktop::check);
    let (detail, problem) = match checked {
        None => (None, Some(backend::not_provided(&config.backend))),
        Some(Ok(detail)) => (Some(detail), None),
        Some(Err(why)) => (None, Some(why)),
    };
    let body = json!({
        "backend": config.backend,
        "plugin": found.as_ref().map(PluginDesktop::plugin),
        "detail": detail,
        "ready": problem.is_none(),
        "problem": problem,
        "permission_mode": permission_mode(config),
        "overlay": config.overlay != Overlay::Off,
    });
    if ctx.json {
        document("computer-use status", &body)?;
    } else {
        let mut out = String::new();
        let _ = writeln!(out, "backend: {}", config.backend);
        if let Some(plugin) = body["plugin"].as_str() {
            let _ = writeln!(out, "plugin: {plugin}");
        }
        match (body["detail"].as_str(), body["problem"].as_str()) {
            (Some(detail), _) => {
                let _ = writeln!(out, "ready: {detail}");
            }
            (_, Some(problem)) => {
                let _ = writeln!(out, "problem: {problem}");
            }
            _ => {}
        }
        let _ = writeln!(out, "permission mode: {}", permission_mode(config));
        print_raw(&out)?;
    }
    Ok(u8::from(body["ready"] != true))
}

fn report(ctx: &Context<'_>, outcome: &doctor::Outcome) -> Result<u8, Error> {
    match outcome {
        doctor::Outcome::Unreachable(why) => Err(Error::new(
            &codes::COMPUTER_USE_UNAVAILABLE,
            why.clone(),
        )
        .with_hint("run `prometheus computer-use install`, or set computer_use.driver_path")),
        doctor::Outcome::Report(rows) => {
            if ctx.json {
                document(
                    "computer-use doctor",
                    &json!({"checks": rows, "ok": outcome.exit_code() == 0}),
                )?;
            } else {
                print_raw(&doctor::render(rows))?;
            }
            Ok(outcome.exit_code())
        }
    }
}

/// The grants of this platform, asking the driver on macOS.
fn grants(scope: &ProfileScope, env: &[(String, OsString)]) -> Result<Vec<platform::Grant>, Error> {
    let config = &scope.config().computer_use;
    let facts = platform::Facts::of(&|name: &str| {
        env.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    });
    let report = if facts.os == "macos" {
        let daemon = DaemonConfig::load(scope.home())?;
        let driver = binary::installed(scope.home(), config, &daemon)
            .map_err(|why| Error::new(&codes::COMPUTER_USE_UNAVAILABLE, why))?
            .ok_or_else(|| {
                Error::new(
                    &codes::COMPUTER_USE_UNAVAILABLE,
                    format!("{RUNTIME} is not installed: {INSTALL_HINT}"),
                )
            })?;
        Some(
            doctor::permissions_report(&driver.path, env)
                .map_err(|why| Error::new(&codes::COMPUTER_USE_UNAVAILABLE, why))?,
        )
    } else {
        None
    };
    Ok(platform::grants(config, &facts, report.as_ref()))
}

fn permissions(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    env: &[(String, OsString)],
    command: &PermissionsCommand,
) -> Result<u8, Error> {
    let grants = grants(scope, env)?;
    match command {
        PermissionsCommand::Status => permissions_status(ctx, &grants),
        PermissionsCommand::Grant => permissions_grant(env, &grants),
    }
}

fn permissions_status(ctx: &Context<'_>, grants: &[platform::Grant]) -> Result<u8, Error> {
    if ctx.json {
        document(
            "computer-use permissions status",
            &json!({"grants": grants}),
        )?;
    } else {
        let mut out = String::new();
        for grant in grants {
            let state = if grant.granted { "granted" } else { "missing" };
            let _ = writeln!(out, "{}: {state}", grant.name);
            if !grant.granted {
                let _ = writeln!(out, "  {}", grant.hint);
            }
        }
        print_raw(&out)?;
    }
    Ok(u8::from(grants.iter().any(|g| !g.granted)))
}

/// Opens the settings pane of each missing grant (macOS).
fn permissions_grant(env: &[(String, OsString)], grants: &[platform::Grant]) -> Result<u8, Error> {
    let panes: Vec<&str> = grants
        .iter()
        .filter(|g| !g.granted)
        .filter_map(|g| platform::settings_pane(g.name))
        .collect();
    if panes.is_empty() {
        eprintln!("nothing to grant here");
        return Ok(0);
    }
    let opener = env
        .iter()
        .find(|(k, _)| k == "PATH")
        .and_then(|(_, p)| {
            pmt_tools::terminal::shell::find_all("open", p, None)
                .into_iter()
                .next()
        })
        .unwrap_or_else(|| "/usr/bin/open".into());
    for pane in panes {
        eprintln!(
            "opening {pane}: grant it to CuaDriver ({})",
            crate::computer_use::identity::BUNDLE_ID
        );
        let status = std::process::Command::new(&opener)
            .arg(pane)
            .status()
            .map_err(|e| {
                Error::new(
                    &codes::COMPUTER_USE_UNAVAILABLE,
                    format!("cannot open {pane}: {e}"),
                )
            })?;
        if !status.success() {
            return Ok(1);
        }
    }
    Ok(0)
}
