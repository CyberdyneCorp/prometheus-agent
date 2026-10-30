//! `prometheus auth` (spec: model-providers, Credential management
//! commands; Subscription and OAuth sign-in registry; OpenAI Codex ChatGPT
//! plan sign-in): API keys enter through a hidden prompt or stdin, sign-ins
//! run the provider's device-code or browser flow, and listings show
//! provider, label, kind, source and status — never key or token material.

mod add;
mod import;
mod list;
mod remove;

use std::process::{Command, Stdio};

use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_core::error::Error;
use pmt_providers::ProviderRegistry;
use pmt_providers::auth::{Credentials, SignInUi};

pub(crate) use add::sign_in_default;

use crate::cli::AuthCommand;
use crate::context::Context;

/// Runs one `auth` subcommand and returns its exit code.
///
/// # Errors
///
/// The command's failure; the caller prints it and exits with its code.
pub fn run(command: &AuthCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        AuthCommand::Add {
            provider,
            kind,
            label,
            browser,
            device,
            argv_key,
        } => add::run(
            ctx,
            &add::Args {
                provider,
                kind: *kind,
                label: label.as_deref(),
                browser: *browser,
                device: *device,
                argv_key,
            },
        ),
        AuthCommand::List => list::list(ctx),
        AuthCommand::Status { provider } => list::status(ctx, provider),
        AuthCommand::Remove {
            provider,
            credential,
        } => remove::run(ctx, provider, credential),
        AuthCommand::Import {
            source: _,
            path,
            label,
        } => import::codex_cli(ctx, path.as_deref(), label.as_deref()),
    }
}

/// The profile's registry and sign-ins.
struct Profile {
    scope: ProfileScope,
    registry: ProviderRegistry,
    credentials: Credentials,
}

impl Profile {
    fn open(ctx: &Context<'_>) -> Result<Self, Error> {
        let scope = ctx.scope()?;
        let registry = ProviderRegistry::for_profile(scope.config(), Vec::new())?;
        let credentials = Credentials::new(scope.secrets().store().clone());
        Ok(Self {
            scope,
            registry,
            credentials,
        })
    }
}

/// Sign-in messages on stderr (stdout stays for `--json`).
struct TerminalUi<'a> {
    launch: &'a LaunchEnv,
}

impl SignInUi for TerminalUi<'_> {
    fn say(&self, text: &str) {
        eprintln!("{text}");
    }

    fn open_browser(&self, url: &str) -> bool {
        let mut opener = if cfg!(target_os = "macos") {
            Command::new("open")
        } else if cfg!(windows) {
            let mut cmd = Command::new("rundll32");
            cmd.arg("url.dll,FileProtocolHandler");
            cmd
        } else {
            Command::new("xdg-open")
        };
        // The opener gets the launch environment minus every secret.
        opener
            .env_clear()
            .envs(self.launch.inherited_vars())
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        opener.spawn().is_ok()
    }
}
