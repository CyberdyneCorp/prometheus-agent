//! `prometheus lsp status|list|install|install-all|restart|which` (spec:
//! lsp-diagnostics, LSP CLI).
//!
//! `status` lists the profile's running servers (every process of the
//! profile writes their entries); `list` the registry with install state;
//! `install` installs an `npm`, `go` or `pip` server into
//! `<PROMETHEUS_HOME>/runtimes/lsp/` and prints how to install a `manual`
//! one; `restart` stops servers and clears broken marks in every process
//! of the profile; `which` prints the binary a server would run.

use std::fmt::Write as _;

use clap::Subcommand;
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_rpc::contracts::{LspServerStatus, LspStatus};
use pmt_tools::lsp::install::{Installs, Tools};
use pmt_tools::lsp::registry::{Install, Registry, Server};
use pmt_tools::lsp::status::{self, Status};
use serde::Serialize;

use crate::context::Context;
use crate::model_cmd::block_on;
use crate::output::{document, print_raw};

/// `prometheus lsp` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum LspCommand {
    /// Running language servers: workspace, pid, uptime and state.
    Status,
    /// The server registry with install state.
    List {
        /// Only servers whose binary is found.
        #[arg(long)]
        installed_only: bool,
    },
    /// Install an npm, go or pip server (prints instructions for others).
    Install {
        /// The server id (`prometheus lsp list`).
        id: String,
    },
    /// Install every npm, go and pip server.
    InstallAll,
    /// Stop running servers and clear broken marks (every server, or one).
    Restart {
        /// The server id.
        id: Option<String>,
    },
    /// Print the binary a server runs; exits 66 when there is none.
    Which {
        /// The server id.
        id: String,
    },
}

/// Runs one `lsp` subcommand.
///
/// # Errors
///
/// The profile cannot be opened, an unknown id (2), a missing binary
/// (66), a failed install (69).
pub fn run(command: &LspCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    match command {
        LspCommand::Status => show_status(&scope, ctx.json),
        LspCommand::List { installed_only } => list(&scope, ctx, *installed_only),
        LspCommand::Install { id } => install(&scope, ctx, Some(id)),
        LspCommand::InstallAll => install(&scope, ctx, None),
        LspCommand::Restart { id } => restart(&scope, id.as_deref()),
        LspCommand::Which { id } => which(&scope, ctx, id),
    }
}

/// The running servers of `scope` (also RPC `lsp.status`).
#[must_use]
pub fn status(scope: &ProfileScope) -> LspStatus {
    let now = status::now_secs();
    LspStatus {
        servers: Status::of_profile(scope.dir())
            .live()
            .into_iter()
            .map(|entry| LspServerStatus {
                uptime_secs: now.saturating_sub(entry.started),
                id: entry.id,
                workspace: entry.workspace,
                pid: entry.pid,
                state: entry.state,
                trusted_only: entry.trusted_only,
                session_id: entry.session_id,
            })
            .collect(),
    }
}

fn show_status(scope: &ProfileScope, json: bool) -> Result<u8, Error> {
    let body = status(scope);
    if json {
        return document("lsp status", &body);
    }
    if body.servers.is_empty() {
        return print_raw("no running language servers\n");
    }
    let mut out = String::new();
    for server in &body.servers {
        let trust = if server.trusted_only {
            "  trusted workspaces only"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "{}  {}  pid {}  up {}s  {}{trust}",
            server.id, server.workspace, server.pid, server.uptime_secs, server.state
        );
    }
    print_raw(&out)
}

fn registry_and_installs(scope: &ProfileScope, ctx: &Context<'_>) -> (Registry, Installs) {
    let path = ctx.launch.inherited("PATH").map(ToOwned::to_owned);
    (
        Registry::new(&scope.config().lsp),
        Installs::of_home(scope.home(), path),
    )
}

fn server<'a>(registry: &'a Registry, id: &str) -> Result<&'a Server, Error> {
    registry.get(id).ok_or_else(|| {
        let why = if registry.is_disabled(id) {
            format!("language server '{id}' is disabled by lsp.servers.{id}.disabled")
        } else {
            format!("unknown language server '{id}'")
        };
        Error::new(&codes::LSP_UNKNOWN_SERVER, why).with_hint("`prometheus lsp list` shows the ids")
    })
}

#[derive(Serialize)]
struct Listed {
    servers: Vec<ListedServer>,
}

#[derive(Serialize)]
struct ListedServer {
    id: String,
    description: String,
    method: &'static str,
    executes_project_code: bool,
    custom: bool,
    /// `managed`, `path` or `command`; absent when not installed.
    source: Option<&'static str>,
    path: Option<String>,
}

fn list(scope: &ProfileScope, ctx: &Context<'_>, installed_only: bool) -> Result<u8, Error> {
    let (registry, installs) = registry_and_installs(scope, ctx);
    let strategy = scope.config().lsp.install_strategy;
    let servers: Vec<ListedServer> = registry
        .servers()
        .iter()
        .map(|server| {
            let found = installs.resolve(server, strategy);
            ListedServer {
                id: server.id.clone(),
                description: server.description.clone(),
                method: server.install.method(),
                executes_project_code: server.executes_project_code,
                custom: server.custom,
                source: found.as_ref().map(|f| f.source.as_str()),
                path: found.map(|f| f.program.display().to_string()),
            }
        })
        .filter(|s| !installed_only || s.path.is_some())
        .collect();
    if ctx.json {
        return document("lsp list", &Listed { servers });
    }
    let mut out = String::new();
    for server in &servers {
        let state = server.source.map_or_else(
            || "not installed".to_owned(),
            |s| format!("installed ({s})"),
        );
        let trust = if server.executes_project_code {
            "  trusted workspaces only"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "{:<28} {:<7} {:<22} {}{trust}",
            server.id, server.method, state, server.description
        );
    }
    print_raw(&out)
}

/// Installs one server (`Some(id)`) or every installable one.
fn install(scope: &ProfileScope, ctx: &Context<'_>, id: Option<&str>) -> Result<u8, Error> {
    let (registry, installs) = registry_and_installs(scope, ctx);
    let targets: Vec<&Server> = match id {
        Some(id) => vec![server(&registry, id)?],
        None => registry.servers().iter().filter(|s| !s.custom).collect(),
    };
    let policies = crate::runtimes_cmd::browser::for_home(
        scope.home(),
        &scope.config().network,
        None,
        ctx.launch.inherited("PATH"),
    )?;
    let manager = scope.config().lsp.package_manager;
    let mut failed = Vec::new();
    for server in targets {
        if let Install::Manual { hint } = server.install {
            print_raw(&format!("{}: install it manually: {hint}\n", server.id))?;
            continue;
        }
        let outcome = block_on(install_one(&installs, server, &policies, manager))?;
        match outcome {
            Ok(path) => {
                print_raw(&format!("{}: installed {}\n", server.id, path.display()))?;
            }
            Err(why) => {
                eprintln!("{}: {why}", server.id);
                failed.push(server.id.clone());
            }
        }
    }
    if failed.is_empty() {
        return Ok(0);
    }
    Err(Error::new(
        &codes::LSP_INSTALL,
        format!("not installed: {}", failed.join(", ")),
    ))
}

/// Installs `server` with the managed Node (npm servers) or CPython (pip
/// servers) when the store has or may install them.
async fn install_one(
    installs: &Installs,
    server: &Server,
    policies: &crate::runtimes_cmd::browser::Policies,
    manager: pmt_config::schema::LspPackageManager,
) -> Result<std::path::PathBuf, String> {
    let node = match server.install {
        Install::Npm { .. } => policies.node.executable("node").await.ok(),
        _ => None,
    };
    let python = match server.install {
        Install::Pip { .. } => policies.python.executable("cpython").await.ok(),
        _ => None,
    };
    let tools = Tools {
        manager,
        node: node.as_deref(),
        python: python.as_deref(),
    };
    installs.install(server, &tools).await
}

fn restart(scope: &ProfileScope, id: Option<&str>) -> Result<u8, Error> {
    let files = Status::of_profile(scope.dir());
    files.request_restart(id).map_err(|e| {
        Error::new(
            &codes::HOME_IO,
            format!("cannot write the restart mark: {e}"),
        )
    })?;
    let stopped = files
        .live()
        .iter()
        .filter(|entry| id.is_none_or(|id| entry.id == id))
        .filter(|entry| status::stop(entry))
        .count();
    let what = id.unwrap_or("every server");
    print_raw(&format!(
        "restart requested for {what}; stopped {stopped} running server(s)\n"
    ))
}

fn which(scope: &ProfileScope, ctx: &Context<'_>, id: &str) -> Result<u8, Error> {
    let (registry, installs) = registry_and_installs(scope, ctx);
    let server = server(&registry, id)?;
    let strategy = scope.config().lsp.install_strategy;
    match installs.resolve(server, strategy) {
        Some(found) => print_raw(&format!("{}\n", found.program.display())),
        None => Err(Error::new(
            &codes::LSP_NOT_FOUND,
            format!("no {id} binary found (not installed under runtimes/lsp and not on PATH)"),
        )
        .with_hint(match server.install {
            Install::Manual { hint } => format!("install it manually: {hint}"),
            _ => format!("prometheus lsp install {id}"),
        })),
    }
}
