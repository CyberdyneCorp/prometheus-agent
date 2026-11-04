//! Python plugins in the plugins CLI (spec: python-bridge, Static
//! compatibility audit; Per-plugin pinned environments; Hermes API
//! compatibility level): the audit report of `plugins validate` and of
//! every install, the refusal of an `incompatible` plugin without
//! `--force`, and the lock and environment an install or update builds
//! (the only time the bridge uses the network).

use std::fmt::Write as _;
use std::path::Path;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_plugins::manifest::{Kind, Manifest};
use pmt_plugins::python::PythonLaunch;
use pmt_plugins::python::audit::{self, Issue, Report, Verdict};

use crate::plugins_host;

/// The audit of a python plugin directory.
///
/// # Errors
///
/// When its files cannot be read.
pub(super) fn audit_dir(dir: &Path, manifest: &Manifest) -> Result<Report, Error> {
    audit::audit(dir, &manifest.capabilities.hooks)
}

/// The report as `plugins validate` prints it.
#[must_use]
pub(super) fn report_text(id: &str, report: &Report) -> String {
    let mut text = format!("{id}: {}\n", report.verdict.as_str());
    for finding in &report.findings {
        let what = match finding.issue {
            Issue::UnsupportedApi => "unsupported API",
            Issue::InternalImport => "Hermes internal import",
            Issue::SecretEnvRead => "secret read from os.environ (use get_secret)",
            Issue::UnknownHook => "hook with no Prometheus equivalent",
        };
        let place = if finding.line == 0 {
            finding.file.clone()
        } else {
            format!("{}:{}", finding.file, finding.line)
        };
        let path = if finding.blocking {
            "; runs while loading"
        } else {
            ""
        };
        let _ = writeln!(text, "  {} at {place} ({what}{path})", finding.name);
    }
    text
}

/// Audits a staged python plugin before it is installed: prints the
/// report and refuses an `incompatible` one unless `force`. Returns its
/// verdict (`None` for other kinds).
///
/// # Errors
///
/// `bridge.incompatible` (77) naming the blocking APIs.
pub(super) fn check_install(
    dir: &Path,
    manifest: &Manifest,
    force: bool,
) -> Result<Option<Verdict>, Error> {
    if manifest.kind != Kind::Python {
        return Ok(None);
    }
    let report = audit_dir(dir, manifest)?;
    eprint!("{}", report_text(&manifest.id, &report));
    if let Some(problem) = PythonLaunch::of(manifest).compat_problem() {
        return Err(Error::new(&codes::BRIDGE_INCOMPATIBLE, problem));
    }
    if report.verdict == Verdict::Incompatible && !force {
        return Err(Error::new(
            &codes::BRIDGE_INCOMPATIBLE,
            format!(
                "plugin '{}' is incompatible with the python bridge ({})",
                manifest.id,
                report.blocking_names().join(", ")
            ),
        )
        .with_hint("`--force` installs it anyway, disabled"));
    }
    Ok(Some(report.verdict))
}

/// Resolves the installed python plugin's lock and builds its environment
/// (network access). A failure is reported, not fatal: the plugin stays
/// installed and `plugins update` retries.
pub(super) fn setup(ctx: &crate::context::Context<'_>, scope: &ProfileScope, id: &str) {
    let dir = scope.dir().join("plugins").join(id);
    let Ok(manifest) = Manifest::read_any(&dir, &plugins_host::running_version()) else {
        return;
    };
    if manifest.kind != Kind::Python {
        return;
    }
    // The interpreter is installed on first need, under the lazy policy.
    let version = scope.config().python_bridge.python_version;
    if let Err(err) = crate::runtimes_cmd::ensure_python(ctx, version) {
        eprintln!(
            "the python environment of '{id}' was not built: {}",
            err.message()
        );
        return;
    }
    match plugins_host::bridge::prepare::install(scope, id, &dir, &PythonLaunch::of(&manifest)) {
        Ok(env) => eprintln!(
            "built the python environment of '{id}' at {}",
            env.dir.display()
        ),
        Err(err) => eprintln!(
            "the python environment of '{id}' was not built: {}; run `prometheus plugins update {id}`",
            err.message()
        ),
    }
}
