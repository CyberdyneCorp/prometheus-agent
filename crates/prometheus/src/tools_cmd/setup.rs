//! `prometheus tools post-setup <toolset>` (spec: tool-system, Tool
//! configuration summary and post-setup): the setup steps a toolset
//! declares that are not done yet, run with the consent and signature
//! rules of `distribution-and-update`. A managed runtime is installed
//! from this binary's pinned, digest-checked runtime index only after a
//! confirmation (a terminal answer, or `--yes`); a step that needs the
//! user (a backend sign-in) is pointed to, never done silently.
//!
//! Steps are declared by single-purpose toolsets (`browser` installs the
//! browser engine, `search` needs a web backend); a composite toolset
//! (`core`, a platform bundle) has the steps of every declaring toolset
//! whose tools it includes.

use std::ffi::OsStr;
use std::path::PathBuf;

use pmt_config::Config;
use pmt_config::daemon::DaemonConfig;
use pmt_core::error::{Error, codes};
use pmt_tools::ToolRegistry;
use pmt_update::runtimes::Store;
use pmt_update::runtimes::install::Installer;
use pmt_update::runtimes::lazy::human_size;

use crate::context::Context;
use crate::model_cmd::block_on;
use crate::runtimes_cmd::browser::needs_local_browser;

/// One setup step that is not done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Install this managed runtime.
    Runtime(&'static str),
    /// Something only the user can set up, and how.
    Configure {
        /// What is missing.
        what: String,
        /// How to set it up.
        how: String,
    },
}

impl Step {
    /// One line describing the step.
    #[must_use]
    pub fn describe(&self, store: &Store) -> String {
        match self {
            Self::Runtime(name) => match store.pinned(name) {
                Ok((pin, artifact)) => format!(
                    "install {name} {} ({})",
                    pin.version,
                    human_size(artifact.size)
                ),
                Err(err) => format!("install {name}: {}", err.message()),
            },
            Self::Configure { what, how } => format!("{what}: {how}"),
        }
    }
}

/// What the steps are checked against.
pub struct Facts<'a> {
    /// The profile's configuration.
    pub config: &'a Config,
    /// The managed runtime store.
    pub store: &'a Store,
    /// Chromium-family browsers of the host.
    pub host_browsers: Vec<PathBuf>,
    /// A Python interpreter is on `PATH`.
    pub host_python: bool,
    /// Whether the profile holds a secret.
    pub has_secret: &'a (dyn Fn(&str) -> bool + Send + Sync),
}

impl Facts<'_> {
    /// The host facts of this process (`PATH`, fixed browser locations).
    #[must_use]
    pub fn host(path_var: Option<&OsStr>) -> (Vec<PathBuf>, bool) {
        let browsers = pmt_tools::browser::launch::system_candidates(path_var);
        let python = path_var.is_some_and(|path| {
            ["python3", "python"]
                .iter()
                .any(|name| !pmt_tools::terminal::shell::find_all(name, path, None).is_empty())
        });
        (browsers, python)
    }
}

/// The toolsets that declare setup steps.
const DECLARING: &[&str] = &["browser", "search"];

/// The pending steps `toolset` itself declares.
fn declared(toolset: &str, facts: &Facts<'_>) -> Vec<Step> {
    match toolset {
        "browser" => {
            let mut steps = Vec::new();
            if needs_local_browser(&facts.config.browser)
                && facts.store.installed("chromium").is_none()
                && facts.host_browsers.is_empty()
            {
                steps.push(Step::Runtime("chromium"));
            }
            if facts.store.installed("cpython").is_none() && !facts.host_python {
                steps.push(Step::Runtime("cpython"));
            }
            steps
        }
        "search" => {
            let resolved = pmt_tools::web::select::resolve(
                &facts.config.web,
                pmt_tools::web::Capability::Search,
                facts.has_secret,
            );
            match resolved {
                Ok(_) => Vec::new(),
                Err(why) => vec![Step::Configure {
                    what: format!("web search ({why})"),
                    how: "store a Firecrawl key with `prometheus secrets set FIRECRAWL_API_KEY`, \
                          or set `web.searxng.url` and run `prometheus tools provider web_search searxng`"
                        .to_owned(),
                }],
            }
        }
        _ => Vec::new(),
    }
}

/// The steps of `toolset` that are not done: those of every declaring
/// toolset whose tools `toolset` includes.
///
/// # Errors
///
/// An unknown toolset (`registry.unknown_name`).
pub fn pending(
    registry: &ToolRegistry,
    toolset: &str,
    facts: &Facts<'_>,
) -> Result<Vec<Step>, Error> {
    let members = registry.members(toolset)?;
    let mut steps = Vec::new();
    for declaring in DECLARING {
        let Ok(tools) = registry.members(declaring) else {
            continue;
        };
        if !tools.is_empty() && tools.iter().all(|t| members.contains(t)) {
            for step in declared(declaring, facts) {
                if !steps.contains(&step) {
                    steps.push(step);
                }
            }
        }
    }
    Ok(steps)
}

/// The runtimes among `steps`.
#[must_use]
pub fn runtimes(steps: &[Step]) -> Vec<&'static str> {
    steps
        .iter()
        .filter_map(|step| match step {
            Step::Runtime(name) => Some(*name),
            Step::Configure { .. } => None,
        })
        .collect()
}

/// Installs `names` with `installer` (pinned version, digest checked),
/// refusing an excluded runtime before anything is downloaded.
///
/// # Errors
///
/// `runtime.excluded` (1), `runtime.download` (69),
/// `runtime.digest_mismatch` (77), `runtime.io` (74).
pub async fn install(installer: &Installer, names: &[&str]) -> Result<Vec<String>, Error> {
    if let Some(name) = names.iter().find(|n| installer.excluded(n)) {
        return Err(pmt_update::runtimes::install::excluded(name));
    }
    let mut done = Vec::new();
    for name in names {
        let runtime = installer.install(name).await?;
        done.push(format!(
            "{name} {} installed at {}",
            runtime.record.version,
            runtime.dir.display()
        ));
    }
    Ok(done)
}

/// What the command or the configurator asks before installing.
pub enum Consent {
    /// `--yes`, or the configurator's answer.
    Given,
    /// Ask on the terminal (none: exit 4).
    Ask,
}

/// The pending steps of `toolset` in the selected home and profile.
pub struct Survey {
    /// The home's runtime store.
    pub store: Store,
    /// The steps not done yet.
    pub steps: Vec<Step>,
}

/// Checks `toolset`'s steps against the home's store, the profile's
/// configuration and secrets, and this host.
///
/// # Errors
///
/// Unknown toolset (`toolset.unknown`, exit 2), home errors.
pub fn survey(ctx: &Context<'_>, registry: &ToolRegistry, toolset: &str) -> Result<Survey, Error> {
    if registry.toolset(toolset).is_none() {
        return Err(super::unknown_toolset(registry, toolset));
    }
    let scope = ctx.scope()?;
    let store = Store::open(ctx.home()?.root());
    let secrets = scope.secrets().clone();
    let has_secret = move |name: &str| secrets.resolve(name).is_ok();
    let path_var = std::env::var_os("PATH");
    let (host_browsers, host_python) = Facts::host(path_var.as_deref());
    let facts = Facts {
        config: scope.config(),
        store: &store,
        host_browsers,
        host_python,
        has_secret: &has_secret,
    };
    let steps = pending(registry, toolset, &facts)?;
    Ok(Survey { store, steps })
}

/// Runs `toolset`'s pending steps; returns the exit code.
///
/// # Errors
///
/// Unknown toolset (`toolset.unknown`, exit 2), runtime errors, a
/// declined install (`runtime.declined`).
pub fn post_setup(ctx: &Context<'_>, toolset: &str, consent: &Consent) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let registry = super::registry(&scope)?;
    let Survey { store, steps } = survey(ctx, &registry, toolset)?;
    if steps.is_empty() {
        println!("toolset {toolset}: setup complete");
        return Ok(0);
    }
    for step in &steps {
        println!("{toolset}: {}", step.describe(&store));
    }
    let names = runtimes(&steps);
    if !names.is_empty() {
        if !confirmed(ctx, toolset, consent)? {
            return Ok(pmt_core::exit::APPROVAL_REQUIRED);
        }
        let config = DaemonConfig::load(ctx.home()?.root())?;
        let installer = crate::runtimes_cmd::installer(ctx, store.clone(), &config)?;
        for line in block_on(install(&installer, &names))?? {
            println!("{line}");
        }
    }
    if names.len() == steps.len() {
        println!("toolset {toolset}: setup complete; the next session exposes its tools");
    } else {
        println!(
            "toolset {toolset}: finish the steps above; the next session then exposes its tools"
        );
    }
    Ok(0)
}

/// Whether the install may go ahead: given, or answered yes on a
/// terminal. Without a terminal it prints the confirmation error and
/// answers `false` (the command exits 4).
fn confirmed(ctx: &Context<'_>, toolset: &str, consent: &Consent) -> Result<bool, Error> {
    if matches!(consent, Consent::Given) {
        return Ok(true);
    }
    if !crate::prompt::stdin_is_tty() {
        let err = Error::new(
            &codes::TOOLS_SETUP_CONFIRMATION,
            "confirmation required to install runtimes",
        )
        .with_hint(format!(
            "run `prometheus tools post-setup {toolset}` on a terminal, or pass --yes"
        ));
        crate::output::print_error(&err, ctx.json);
        return Ok(false);
    }
    if crate::prompt::confirm("Install?")? {
        Ok(true)
    } else {
        Err(Error::new(
            &codes::RUNTIME_DECLINED,
            "install declined; nothing was downloaded",
        ))
    }
}

#[cfg(test)]
mod tests;
