//! `prometheus acp` (spec: acp-adapter): the Agent Client Protocol server
//! editors launch, as a viewer of the profile's gateway daemon.
//!
//! `--version`, `--check`, `--setup` and `--setup-browser` run before any
//! server starts and exit. Otherwise the process serves ACP on stdio: the
//! protocol stream is a private copy of stdout and the process's own
//! stdout is pointed at stderr first, so only protocol frames can reach
//! the editor, whatever a component or a child prints; logs go to stderr
//! and `<profile>/logs/acp.log`, redacted. Configuration comes from the
//! profile and the launch flags only.

mod host;
mod stdio;

use std::sync::Arc;

use clap::Args;
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};

use crate::Context;

/// `prometheus acp` arguments.
#[derive(Debug, Clone, Args)]
#[allow(clippy::struct_excessive_bools)] // independent launch switches
pub struct AcpArgs {
    /// Print the version and exit.
    #[arg(long)]
    pub version: bool,
    /// Check the profile, the daemon and the model, then exit (0 when
    /// ready, 1 naming the first failing check).
    #[arg(long, conflicts_with_all = ["setup", "setup_browser"])]
    pub check: bool,
    /// Run `prometheus setup`, then offer the browser runtime, and exit.
    #[arg(long, conflicts_with = "setup_browser")]
    pub setup: bool,
    /// Install the managed Chromium runtime for ACP browser tools and exit.
    #[arg(long = "setup-browser")]
    pub setup_browser: bool,
    /// Install without asking (`--setup-browser`).
    #[arg(long, requires = "setup_browser")]
    pub yes: bool,
    /// The model of the sessions this process opens.
    #[arg(short = 'm', long)]
    pub model: Option<String>,
    /// The provider of the sessions this process opens.
    #[arg(long)]
    pub provider: Option<String>,
    /// Approval-gated calls run without asking.
    #[arg(long)]
    pub yolo: bool,
    /// Schema defaults instead of the profile's `config.toml`.
    #[arg(long)]
    pub ignore_user_config: bool,
    /// Serve a client that cannot answer prompts (a channel relay bridge):
    /// approvals follow `approvals.unattended_mode` and the grants.
    #[arg(long)]
    pub unattended: bool,
}

/// What `--check` reports when the browser tools are wanted but missing.
const BROWSER_MISSING: &str = "browser runtime not installed; run 'prometheus acp --setup-browser'";

/// Runs `prometheus acp`.
///
/// # Errors
///
/// Profile resolution errors; the server's own failures.
pub fn run(args: &AcpArgs, ctx: &Context<'_>, safe_mode: bool, json: bool) -> Result<u8, Error> {
    if args.version {
        return Ok(crate::version::run(json, false));
    }
    if args.check {
        return Ok(check(args, ctx));
    }
    let scope = ctx.scope()?;
    if args.setup_browser {
        return host::setup_browser(ctx, &scope, args.yes);
    }
    if args.setup {
        let code = crate::setup_cmd::run(&setup_all(), ctx)?;
        if code != 0 {
            return Ok(code);
        }
        let wanted = crate::prompt::stdin_is_tty()
            && crate::prompt::confirm("Install the browser runtime for ACP sessions?")?;
        return if wanted {
            host::setup_browser(ctx, &scope, true)
        } else {
            Ok(0)
        };
    }
    serve(args, ctx, &scope, safe_mode)
}

fn setup_all() -> crate::cli::SetupArgs {
    crate::cli::SetupArgs {
        section: None,
        platform: None,
        non_interactive: false,
        quick: false,
        reset: false,
        reset_onboarding: false,
        portal: false,
    }
}

/// `--check`: the first failing check, or `Prometheus ACP check OK`.
fn check(args: &AcpArgs, ctx: &Context<'_>) -> u8 {
    match first_failure(args, ctx) {
        None => {
            println!("Prometheus ACP check OK");
            0
        }
        Some(failure) => {
            println!("{failure}");
            1
        }
    }
}

fn first_failure(args: &AcpArgs, ctx: &Context<'_>) -> Option<String> {
    let scope = match ctx.scope() {
        Ok(scope) => scope,
        Err(err) => return Some(err.message().to_owned()),
    };
    if let Err(err) = host::daemon_available(scope.home()) {
        return Some(err.message().to_owned());
    }
    let config = scope.config();
    let model = args.model.is_some() || config.model.default.is_some();
    if !model {
        return Some(format!(
            "no default model configured for profile {}; run 'prometheus setup'",
            scope.name()
        ));
    }
    if wants_browser(&scope, args.unattended)
        && !crate::runtimes_cmd::browser::acp_browser_ready(&scope)
    {
        return Some(BROWSER_MISSING.to_owned());
    }
    None
}

/// Whether the ACP toolset of `scope` includes the browser tools.
fn wants_browser(scope: &ProfileScope, unattended: bool) -> bool {
    let config = scope.config();
    let surface = if unattended {
        config.toolsets.surfaces.acp_unattended.as_ref()
    } else {
        config.toolsets.surfaces.acp.as_ref()
    };
    surface.is_some_and(|names| names.iter().any(|n| n == pmt_tools::browser::TOOLSET))
}

/// Serves ACP on stdio until the editor closes it.
fn serve(
    args: &AcpArgs,
    ctx: &Context<'_>,
    scope: &ProfileScope,
    safe_mode: bool,
) -> Result<u8, Error> {
    let protocol = stdio::protocol_stdout()?;
    if let Some(handle) = pmt_obs::logs::handle() {
        handle.serve_process("acp", scope.name().as_str(), true);
    }
    let options = pmt_acp::Options {
        profile: scope.name().to_string(),
        version: crate::build_info::VERSION.to_owned(),
        unattended: args.unattended,
        launch: pmt_acp::Launch {
            model: args.model.clone(),
            provider: args.provider.clone(),
            yolo: args.yolo,
            safe_mode,
            ignore_user_config: args.ignore_user_config,
        },
        auth: Arc::new(host::ProfileAuth::new(ctx)?),
        fetcher: Arc::new(host::EgressFetcher::new(scope)?),
        drop_first_ack: ctx.launch.test_acp_drop_ack(),
    };
    let connector = Arc::new(host::DaemonConnector {
        home: scope.home().to_path_buf(),
        profile: scope.name().to_string(),
        unattended: args.unattended,
    });
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::new(&codes::OUTPUT_IO, format!("cannot start the runtime: {e}")))?;
    tracing::info!(profile = %scope.name(), unattended = args.unattended, "ACP server started");
    runtime.block_on(pmt_acp::serve(
        tokio::io::stdin(),
        protocol,
        options,
        connector,
    ));
    tracing::info!("ACP client closed the connection");
    Ok(0)
}
