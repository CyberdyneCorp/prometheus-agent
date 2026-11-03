//! How a configured MCP server starts (spec: plugin-system, MCP client
//! servers; tool-system, MCP runtime safety).
//!
//! A `stdio` server runs under the profile's execution sandbox: the
//! capability baseline, its `capabilities` table (network hosts, readable
//! and writable paths, like a plugin grant), reading its program's
//! directory, spawning children and writing its own scratch directory
//! `<profile>/cache/mcp/<server>`. Its environment holds only `PATH`,
//! `HOME`, `LANG`, `SYSTEMROOT` (and on Windows `PATHEXT` and
//! `USERPROFILE`), `TMPDIR` (the scratch directory), its `env` values and
//! its `env_secrets` from the secrets store, so no other secret (a
//! provider key) reaches it. An `http` server is reached through the
//! profile's outbound HTTP client, with `auth_secret` as bearer token.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_config::grants::baseline;
use pmt_config::schema::{McpServerConfig, McpTransport};
use pmt_core::capability::{Capability, CapabilitySet};
use pmt_mcp::Launch;
use pmt_mcp::server::Launcher;
use pmt_net::OutboundPolicy;
use pmt_sandbox::{Paths, Policy, Sandbox};

/// Variables a `stdio` server inherits from this process.
const INHERITED: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "SYSTEMROOT",
    "PATHEXT",
    "USERPROFILE",
];

/// The launcher of server `name`.
pub(super) fn launcher(scope: &ProfileScope, name: &str, server: &McpServerConfig) -> Launcher {
    let (scope, name, server) = (scope.clone(), name.to_owned(), server.clone());
    Arc::new(move || match server.transport {
        McpTransport::Stdio => stdio(&scope, &name, &server),
        McpTransport::Http => http(&scope, &server),
    })
}

/// The scratch directory of server `name`.
fn scratch(scope: &ProfileScope, name: &str) -> PathBuf {
    scope.dir().join("cache/mcp").join(name)
}

fn stdio(scope: &ProfileScope, name: &str, server: &McpServerConfig) -> Result<Launch, String> {
    let config = scope.config();
    let scratch = scratch(scope, name);
    std::fs::create_dir_all(&scratch).map_err(|e| format!("{}: {e}", scratch.display()))?;
    let workspace = std::env::current_dir().unwrap_or_else(|_| scope.dir().to_path_buf());
    let program = program(&server.command);
    let capabilities = baseline(&config.security).union(&declared(server, program.as_deref())?);
    let paths = Paths {
        user_home: std::env::var_os("HOME").map(Into::into).unwrap_or_default(),
        prometheus_home: scope.home().to_path_buf(),
        workspace: workspace.clone(),
        scratch: vec![scratch.clone()],
    };
    let sandbox = Sandbox::new(
        Policy::from_set(&capabilities, &paths),
        &config.security.sandbox,
    )
    .map_err(|e| e.message().to_owned())?;
    let mut command = sandbox
        .command(program.as_deref().unwrap_or(Path::new(&server.command)))
        .map_err(|e| e.message().to_owned())?;
    command
        .args(&server.args)
        .current_dir(&workspace)
        .env_clear();
    for var in INHERITED {
        if let Some(value) = std::env::var_os(var) {
            command.env(var, value);
        }
    }
    command.env("TMPDIR", &scratch);
    command.envs(&server.env);
    for secret in &server.env_secrets {
        let value = scope
            .secrets()
            .resolve(secret)
            .map_err(|e| e.message().to_owned())?;
        command.env(secret, value.expose());
    }
    Ok(Launch::Stdio(tokio::process::Command::from(command)))
}

/// The program `command` names: a path, or found on `PATH`.
fn program(command: &str) -> Option<PathBuf> {
    let path = Path::new(command);
    if path.components().count() > 1 {
        return Some(path.to_path_buf());
    }
    let search = std::env::var_os("PATH")?;
    let pathext = std::env::var_os("PATHEXT");
    pmt_tools::terminal::shell::find_executable(command, &search, pathext.as_deref())
}

/// The server's `capabilities` table as capabilities, plus spawning
/// children and reading its program's directory.
fn declared(server: &McpServerConfig, program: Option<&Path>) -> Result<CapabilitySet, String> {
    let declared = &server.capabilities;
    let place = |path: &str| match path.strip_prefix("$workspace") {
        Some(rest) => format!("<workspace>{rest}"),
        None => path.to_owned(),
    };
    let mut texts = vec!["process.spawn:*".to_owned()];
    texts.extend(declared.network.iter().map(|h| format!("net.connect:{h}")));
    texts.extend(
        declared
            .fs_read
            .iter()
            .map(|p| format!("fs.read:{}", place(p))),
    );
    texts.extend(
        declared
            .fs_write
            .iter()
            .map(|p| format!("fs.write:{}", place(p))),
    );
    if let Some(dir) = program.and_then(Path::parent) {
        texts.push(format!("fs.read:{}/**", dir.display()));
    }
    texts
        .iter()
        .map(|text| Capability::parse(text).map_err(|e| e.message().to_owned()))
        .collect()
}

fn http(scope: &ProfileScope, server: &McpServerConfig) -> Result<Launch, String> {
    let network = &scope.config().network;
    let policy = OutboundPolicy {
        force_ipv4: network.force_ipv4,
        extra_ca_file: network.extra_ca_file.as_ref().map(Into::into),
        proxy: network.proxy.clone(),
    };
    let client = pmt_net::client(&policy).map_err(|e| e.message().to_owned())?;
    let bearer = if server.auth_secret.is_empty() {
        None
    } else {
        let token = scope
            .secrets()
            .resolve(&server.auth_secret)
            .map_err(|e| e.message().to_owned())?;
        Some(token.expose().to_owned())
    };
    Ok(Launch::Http {
        url: server.url.clone(),
        client,
        bearer,
    })
}
