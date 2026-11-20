//! `prometheus secrets bitwarden|onepassword` (spec: configuration-and-
//! profiles, External secret source commands; security-boundaries,
//! External secret sources are opt-in). The sources' lookup lives in
//! `pmt_config::secrets::external`; these commands install the CLIs,
//! check and store the bootstrap tokens and edit the sources' settings.
//! A token is stored only after the CLI accepted it, so a mistyped
//! rotation leaves the working token in place (exit 78).

pub mod args;
mod bitwarden;
mod onepassword;

use std::path::{Path, PathBuf};

use pmt_config::ProfileScope;
use pmt_config::secrets::external;
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;

pub use args::{BitwardenCommand, OnePasswordCommand};

use crate::context::Context;
use crate::prompt;

/// Runs one `secrets bitwarden` subcommand.
///
/// # Errors
///
/// As each subcommand.
pub fn bitwarden(command: &BitwardenCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    bitwarden::run(command, ctx)
}

/// Runs one `secrets onepassword` subcommand.
///
/// # Errors
///
/// As each subcommand.
pub fn onepassword(command: &OnePasswordCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    onepassword::run(command, ctx)
}

/// One external source's CLI.
struct Cli {
    /// The runtime (and executable) name: `bws` or `op`.
    name: &'static str,
    /// The variable its token travels in.
    token_var: &'static str,
}

const BWS: Cli = Cli {
    name: "bws",
    token_var: "BWS_ACCESS_TOKEN",
};

const OP: Cli = Cli {
    name: "op",
    token_var: "OP_SERVICE_ACCOUNT_TOKEN",
};

impl Cli {
    /// The managed install, when there is one.
    fn managed(&self, home: &Path) -> Option<PathBuf> {
        let program = external::program(home, self.name);
        program.is_absolute().then_some(program)
    }

    /// The managed CLI, installing the pinned one after confirmation when
    /// it is missing; `Err(code)` when the install was not confirmed.
    fn ensure(&self, ctx: &Context<'_>, yes: bool) -> Result<Result<PathBuf, u8>, Error> {
        let home = ctx.home()?;
        if let Some(found) = self.managed(home.root()) {
            return Ok(Ok(found));
        }
        if let Some(code) = self.install(ctx, yes)? {
            return Ok(Err(code));
        }
        self.managed(home.root()).map(Ok).ok_or_else(|| {
            Error::new(
                &codes::RUNTIME_IO,
                format!(
                    "{} was installed but is not in the runtime store",
                    self.name
                ),
            )
        })
    }

    /// Installs the pinned CLI into `<home>/runtimes/`; `Some(code)` when
    /// it was not confirmed.
    fn install(&self, ctx: &Context<'_>, yes: bool) -> Result<Option<u8>, Error> {
        let home = ctx.home()?;
        let config = pmt_config::DaemonConfig::load(home.root())?;
        let store = pmt_update::runtimes::Store::open(home.root());
        let (version, size) = {
            let (pin, artifact) = store.pinned(self.name)?;
            (pin.version.clone(), artifact.size)
        };
        let what = format!(
            "install {} {version} ({}) into {}",
            self.name,
            pmt_update::runtimes::lazy::human_size(size),
            store.root().display()
        );
        if !yes {
            if !prompt::stdin_is_tty() {
                eprintln!("error: {what} needs confirmation; rerun with -y");
                return Ok(Some(pmt_core::exit::APPROVAL_REQUIRED));
            }
            if !prompt::confirm(&format!("{what}?"))? {
                eprintln!("not installed");
                return Ok(Some(1));
            }
        }
        let installer = crate::runtimes_cmd::installer(ctx, store, &config);
        crate::model_cmd::block_on(installer.install(self.name))??;
        eprintln!("installed {} {version}", self.name);
        Ok(None)
    }

    /// Runs the CLI with `token`; its stdout.
    fn run(&self, program: &Path, args: &[&str], token: &SecretString) -> Result<Vec<u8>, Error> {
        external::run(program, args, self.token_var, token).map_err(|why| refused(self.name, &why))
    }

    /// `<cli> --version`, when it answers.
    fn version(program: &Path) -> Option<String> {
        let out = std::process::Command::new(program)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }
}

/// A token the CLI refused, or a CLI that failed (exit 78).
fn refused(cli: &str, why: &str) -> Error {
    Error::new(&codes::SECRET_SOURCE_FAILED, format!("{cli}: {why}"))
}

/// The stored token named `name`.
fn stored_token(scope: &ProfileScope, name: &str) -> Result<SecretString, Error> {
    scope.secrets().store().get(name)?.ok_or_else(|| {
        Error::new(
            &codes::SECRET_SOURCE_FAILED,
            format!("the access token secret {name} is not set"),
        )
        .with_hint("run the source's setup or token command")
    })
}

/// Reads a new token (hidden prompt, else stdin), has `check` validate it
/// and only then stores it under `name`.
fn replace_token(
    scope: &ProfileScope,
    name: &str,
    check: impl FnOnce(&SecretString) -> Result<(), Error>,
) -> Result<(), Error> {
    let token = prompt::read_secret(name)?;
    check(&token).map_err(|err| {
        Error::new(
            &codes::SECRET_SOURCE_FAILED,
            format!("{}; the stored token is unchanged", err.message()),
        )
    })?;
    scope.secrets().store().set(name, &token)?;
    eprintln!("{name} stored in profile '{}'", scope.name());
    Ok(())
}

/// Sets `pairs` in the profile's configuration in one write.
fn configure(scope: &ProfileScope, pairs: &[(&str, &str)]) -> Result<(), Error> {
    scope.profile().config_file().set_all(pairs)
}

/// Names another source (managed, the store, the environment) already
/// supplies: they shadow an external source's value of the same name.
fn shadowing(scope: &ProfileScope) -> Result<std::collections::BTreeSet<String>, Error> {
    Ok(scope
        .secrets()
        .list()?
        .into_iter()
        .map(|row| row.name)
        .collect())
}

/// `name  added|shadowed` lines.
fn preview_line(name: &str, shadowed: bool, extra: &str) -> String {
    format!(
        "{name}  {}{extra}\n",
        if shadowed {
            "shadowed (another source wins)"
        } else {
            "added"
        }
    )
}

/// A TOML array literal of `names`.
fn toml_list(names: &[String]) -> String {
    let quoted: Vec<String> = names.iter().map(|n| format!("{n:?}")).collect();
    format!("[{}]", quoted.join(", "))
}
