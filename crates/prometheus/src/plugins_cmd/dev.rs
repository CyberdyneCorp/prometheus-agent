//! `prometheus plugins dev <path> [--watch]` (spec: plugin-system, Plugin
//! development loop): serves a local plugin directory to the profile as
//! `dev:<id>` while the command runs, without installing or signing it.
//! The capabilities the developer approves at start are granted to each
//! build (by its digest) and taken back when the command ends; with
//! `--watch` every source change rebuilds it (the manifest's `build`
//! command) and the processes a developer works in (`chat`, the TUI)
//! reload it at their next hook, under the reload rules. Load errors and
//! the capabilities it asks for but was not granted are printed after
//! each build. Gateway and cron processes never load it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use clap::Args;
use pmt_config::ProfileScope;
use pmt_config::grants::GrantsFile;
use pmt_core::capability::CapabilitySet;
use pmt_core::error::{Error, codes};
use pmt_plugins::discovery::DEV_PREFIX;
use pmt_plugins::grants::{Identity, select};
use pmt_plugins::host::state::StateDir;
use pmt_plugins::load::State;
use pmt_plugins::manifest::{Manifest, describe};

use crate::context::Context;
use crate::plugins_host::{self, dev};

/// `plugins dev` flags.
#[derive(Debug, Clone, Args)]
pub struct DevArgs {
    /// The plugin directory.
    pub path: PathBuf,
    /// Rebuild and reload on every source change.
    #[arg(long)]
    pub watch: bool,
    /// Approve `all` requested capabilities, or a comma list, without
    /// asking.
    #[arg(long, value_name = "CAPS")]
    pub grant: Option<String>,
}

/// How often the directory is looked at.
const POLL: Duration = Duration::from_millis(300);

/// Directories a change in which is no source change.
const IGNORED: &[&str] = &["target", ".git", "node_modules", "__pycache__"];

/// Runs the loop until Ctrl+C.
///
/// # Errors
///
/// `plugins.discovery` for an unreadable manifest, `home.io`.
pub(super) fn dev(args: &DevArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    dev::allow();
    let scope = ctx.scope()?;
    let dir = std::fs::canonicalize(&args.path)
        .map_err(|e| Error::io(&codes::PLUGINS_DISCOVERY, &args.path, &e))?;
    let manifest = Manifest::read_any(&dir, &plugins_host::running_version())?;
    let id = manifest.id.clone();
    let requested = manifest
        .capabilities
        .requested(StateDir::new(scope.dir(), &id).root())?;
    let approved = approve(&id, &requested, args.grant.as_deref())?;
    let mut served = Served {
        scope: &scope,
        id: id.clone(),
        dir: dir.clone(),
        approved,
        registration: dev::Registration::of(&dir),
        digest: None,
    };
    served.rebuild(&manifest)?;
    println!(
        "serving {DEV_PREFIX}{id} from {}{} (Ctrl+C stops)",
        dir.display(),
        if args.watch {
            "; watching for changes"
        } else {
            ""
        }
    );
    crate::model_cmd::block_on(serve(&mut served, args.watch))?;
    Ok(pmt_core::exit::SUCCESS)
}

/// Serves until Ctrl+C; with `watch`, every source change rebuilds.
async fn serve(served: &mut Served<'_>, watch: bool) {
    let mut seen = sources(&served.dir);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return,
            () = tokio::time::sleep(POLL) => {}
        }
        if !watch || sources(&served.dir) == seen {
            continue;
        }
        match Manifest::read_any(&served.dir, &plugins_host::running_version()) {
            Ok(manifest) => {
                if let Err(err) = served.rebuild(&manifest) {
                    println!("{DEV_PREFIX}{} not reloaded: {}", served.id, err.message());
                }
            }
            Err(err) => println!(
                "{DEV_PREFIX}{} manifest error: {}",
                served.id,
                err.message()
            ),
        }
        // What the build wrote is no source change.
        seen = sources(&served.dir);
    }
}

/// The capabilities granted for the session: `--grant`, else asked one
/// by one on a terminal, else none.
fn approve(
    id: &str,
    requested: &CapabilitySet,
    grant: Option<&str>,
) -> Result<CapabilitySet, Error> {
    if let Some(list) = grant {
        return select(requested, list);
    }
    if requested.is_empty() {
        return Ok(CapabilitySet::new());
    }
    if !crate::prompt::stdin_is_tty() {
        eprintln!(
            "{DEV_PREFIX}{id} asks for capabilities; none are granted without --grant (all, or a comma list)"
        );
        return Ok(CapabilitySet::new());
    }
    let mut granted = Vec::new();
    for capability in requested.iter() {
        if crate::prompt::confirm(&format!(
            "Grant {DEV_PREFIX}{id}: {}?",
            describe(capability)
        ))? {
            granted.push(capability.clone());
        }
    }
    Ok(granted.into_iter().collect())
}

/// The served plugin: its registration and the build its grants follow.
struct Served<'a> {
    scope: &'a ProfileScope,
    id: String,
    dir: PathBuf,
    approved: CapabilitySet,
    registration: dev::Registration,
    /// The digest the current grants are for.
    digest: Option<String>,
}

impl Served<'_> {
    /// Builds, grants the approved capabilities to the new digest, bumps
    /// the registration (sessions reload at their next hook) and prints
    /// how the build loads here.
    fn rebuild(&mut self, manifest: &Manifest) -> Result<(), Error> {
        if let Some(command) = &manifest.build {
            build(&self.dir, command)?;
        }
        let digest = pmt_plugins::digest::of_dir(&self.dir)?;
        let grants = GrantsFile::at(self.scope.dir().join("grants.toml"));
        self.revoke(&grants)?;
        if !self.approved.is_empty() {
            let identity = Identity {
                id: self.id.clone(),
                digest: digest.clone(),
            };
            pmt_plugins::grants::approve(&grants, &identity, &self.approved, "dev")?;
        }
        self.digest = Some(digest);
        self.registration.generation += 1;
        self.registration.save(self.scope.dir(), &self.id)?;
        self.report();
        Ok(())
    }

    /// Takes back the grants of the previous build.
    fn revoke(&self, grants: &GrantsFile) -> Result<(), Error> {
        let Some(digest) = &self.digest else {
            return Ok(());
        };
        let subject = Identity {
            id: self.id.clone(),
            digest: digest.clone(),
        }
        .subject();
        for grant in grants
            .load()?
            .grants
            .into_iter()
            .filter(|g| g.subject == subject)
        {
            grants.revoke(grant.id)?;
        }
        Ok(())
    }

    /// Loads the profile's plugins here and prints the dev plugin's state,
    /// a load error, and what it asks for but was not granted.
    fn report(&self) {
        let loaded = plugins_host::reload(self.scope, &self.dir).current();
        let name = format!("{DEV_PREFIX}{}", self.id);
        let Some(status) = loaded.statuses.iter().find(|s| s.id == name) else {
            println!("{name} was not discovered");
            return;
        };
        match (status.state, &status.problem) {
            (State::Loaded, _) => {
                println!("{name} loaded (build {})", self.registration.generation);
            }
            (_, Some(problem)) => println!("{name} {}: {problem}", status.state.as_str()),
            (state, None) => println!("{name} {}", state.as_str()),
        }
        let requested = Manifest::read_any(&self.dir, &plugins_host::running_version())
            .and_then(|m| {
                m.capabilities
                    .requested(StateDir::new(self.scope.dir(), &self.id).root())
            })
            .unwrap_or_default();
        for capability in requested.iter().filter(|c| !self.approved.allows(c)) {
            println!("{name} capability denied: {}", describe(capability));
        }
    }
}

impl Drop for Served<'_> {
    fn drop(&mut self) {
        dev::unregister(self.scope.dir(), &self.id);
        let grants = GrantsFile::at(self.scope.dir().join("grants.toml"));
        if let Err(err) = self.revoke(&grants) {
            eprintln!(
                "warning: the dev grants were not revoked: {}",
                err.message()
            );
        }
    }
}

/// Runs the manifest's `build` command in `dir`.
fn build(dir: &Path, command: &str) -> Result<(), Error> {
    #[cfg(windows)]
    let mut shell = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", command]);
        c
    };
    #[cfg(not(windows))]
    let mut shell = {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", command]);
        c
    };
    let out = shell.current_dir(dir).output().map_err(|e| {
        Error::new(
            &codes::PLUGINS_LOAD,
            format!("build `{command}` did not start: {e}"),
        )
    })?;
    if out.status.success() {
        return Ok(());
    }
    Err(Error::new(
        &codes::PLUGINS_LOAD,
        format!(
            "build `{command}` failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    ))
}

/// Every source file of `dir` with its size and modification time.
fn sources(dir: &Path) -> BTreeMap<PathBuf, (u64, Option<SystemTime>)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next).into_iter().flatten().flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                let name = entry.file_name();
                if !IGNORED.iter().any(|i| name.to_string_lossy() == *i) {
                    stack.push(path);
                }
            } else {
                out.insert(path, (meta.len(), meta.modified().ok()));
            }
        }
    }
    out
}
