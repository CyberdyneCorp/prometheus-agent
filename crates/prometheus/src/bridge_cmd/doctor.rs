//! `prometheus bridge doctor` (spec: python-bridge, Bridge diagnostics):
//! the managed interpreter and uv, every enabled python plugin's
//! environment against its `bridge.lock`, and how strongly the OS sandbox
//! confines sidecars. Exit 1 when any check fails.

use std::fmt::Write as _;

use pmt_config::ProfileScope;
use pmt_core::error::Error;
use pmt_plugins::python::PythonLaunch;
use pmt_pybridge::env;
use pmt_pybridge::runtime::{self, Uv};
use pmt_sandbox::{Paths, Policy, Sandbox};

use crate::context::Context;
use crate::output::print_raw;
use crate::plugins_host::bridge::prepare::{envs_root, lock_dir, requirements};

struct Report {
    text: String,
    failed: bool,
}

impl Report {
    fn ok(&mut self, line: &str) {
        let _ = writeln!(self.text, "✓ {line}");
    }

    fn fail(&mut self, line: &str) {
        self.failed = true;
        let _ = writeln!(self.text, "✗ {line}");
    }

    fn warn(&mut self, line: &str) {
        let _ = writeln!(self.text, "! {line}");
    }
}

pub(super) fn run(ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let mut report = Report {
        text: String::new(),
        failed: false,
    };
    let uv = toolchain(&scope, &mut report);
    if let Some((uv, python)) = &uv {
        environments(&scope, uv, python, &mut report);
    }
    sandbox(&scope, &mut report);
    print_raw(&report.text)?;
    Ok(u8::from(report.failed))
}

fn toolchain(scope: &ProfileScope, report: &mut Report) -> Option<(Uv, runtime::Interpreter)> {
    let version = scope.config().python_bridge.python_version;
    let python = match runtime::require(scope.home(), version) {
        Ok(python) => {
            report.ok(&format!(
                "runtime: CPython {} ({})",
                python.version,
                python.exe.display()
            ));
            Some(python)
        }
        Err(err) => {
            report.fail(&format!("runtime: {}", err.message()));
            None
        }
    };
    let uv = Uv::of(scope.home());
    match &uv {
        Some(uv) => report.ok(&format!("uv: {}", uv.exe.display())),
        None => report.fail(&format!(
            "uv: not installed; run prometheus runtimes install python@{version}"
        )),
    }
    Some((uv?, python?))
}

fn environments(scope: &ProfileScope, uv: &Uv, python: &runtime::Interpreter, report: &mut Report) {
    let version = crate::plugins_host::running_version();
    let blocked = crate::plugins_host::catalog::kill_list(scope);
    let cwd = std::env::current_dir().unwrap_or_else(|_| scope.dir().to_path_buf());
    let inputs = crate::plugins_host::inputs(scope, &cwd, &version, &blocked);
    let surveyed = pmt_plugins::load::survey(&inputs);
    for plugin in pmt_plugins::load::bridged(&surveyed, &inputs) {
        let id = &plugin.manifest.id;
        let lock = lock_dir(scope, id).join(env::LOCK_FILE);
        if let Err(err) = requirements(scope, &plugin.dir, &PythonLaunch::of(&plugin.manifest)) {
            report.fail(&format!("{id}: {}", err.message()));
            continue;
        }
        if !lock.is_file() {
            report.fail(&format!(
                "{id}: no bridge.lock; run prometheus plugins update {id}"
            ));
            continue;
        }
        let Some(built) = env::existing(&envs_root(scope), id, &lock, python) else {
            report.warn(&format!(
                "{id}: environment not built yet (it is built offline at the next session)"
            ));
            continue;
        };
        match env::verify(uv, &built, &lock) {
            Ok(mismatches) if mismatches.is_empty() => {
                report.ok(&format!("{id}: environment matches bridge.lock"));
            }
            Ok(mismatches) => {
                for m in mismatches {
                    let have = m.installed.as_deref().unwrap_or("missing");
                    report.fail(&format!(
                        "{id}: package {} is {have} but bridge.lock pins {}",
                        m.package, m.locked
                    ));
                }
            }
            Err(err) => report.fail(&format!("{id}: {}", err.message())),
        }
    }
}

fn sandbox(scope: &ProfileScope, report: &mut Report) {
    let paths = Paths {
        user_home: std::env::var_os("HOME").map(Into::into).unwrap_or_default(),
        prometheus_home: scope.home().to_path_buf(),
        workspace: scope.dir().join("plugin-data"),
        scratch: Vec::new(),
    };
    let policy =
        match Policy::from_capabilities(&["fs.read:<toolchain>", "net.connect:*:1"], &paths) {
            Ok(policy) => policy,
            Err(err) => return report.fail(&format!("sandbox: {}", err.message())),
        };
    match Sandbox::new(policy, &scope.config().security.sandbox) {
        Ok(sandbox) => {
            for check in sandbox.report().doctor_checks() {
                let line = format!("sandbox {}", check.message);
                if check.status == pmt_core::doctor::Status::Ok {
                    report.ok(&line);
                } else {
                    report.warn(&line);
                }
            }
        }
        Err(err) => report.fail(&format!("sandbox: {}", err.message())),
    }
}
