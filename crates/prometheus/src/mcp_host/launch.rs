//! How a configured MCP server starts (spec: plugin-system, MCP client
//! servers; tool-system, MCP runtime safety).
//!
//! A `stdio` server runs under the profile's execution sandbox: the
//! capability baseline, its `capabilities` table (network hosts, readable
//! and writable paths, like a plugin grant), reading its program's
//! directory, spawning children and writing its own scratch directory
//! `<profile>/cache/mcp/<server>`; it reaches the network only through a
//! loopback egress proxy under the profile's `security.egress` policy and
//! its declared hosts. Its environment holds only `PATH`,
//! `HOME`, `LANG`, `SYSTEMROOT` (and on Windows `PATHEXT` and
//! `USERPROFILE`), `TMPDIR` (the scratch directory), its `env` values and
//! its `env_secrets` from the secrets store, so no other secret (a
//! provider key) reaches it. An `http` server is reached through the
//! profile's outbound HTTP client (presenting `client_cert` when set),
//! with its `headers`, its identity header and, by `auth`, the
//! `auth_secret` bearer token or the OAuth access token. `${NAME}`
//! placeholders in the command, arguments, environment, URL and headers
//! are filled at each start from the secrets store and the context
//! variables (spec: plugin-system, MCP mTLS and identity header; MCP
//! catalog entries).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use pmt_config::ProfileScope;
use pmt_config::grants::baseline;
use pmt_config::schema::{McpAuth, McpServerConfig, McpTransport};
use pmt_core::capability::{Capability, CapabilitySet};
use pmt_mcp::Launch;
use pmt_mcp::server::Launcher;
use pmt_net::OutboundPolicy;
use pmt_net::egress::Proxy;
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

/// The launcher of server `name`. A `stdio` server's egress proxy lives
/// until its next start; its refusals belong to the session whose call is
/// in flight (`caller`).
pub(super) fn launcher(
    scope: &ProfileScope,
    name: &str,
    server: &McpServerConfig,
    caller: crate::egress::Caller,
) -> Launcher {
    let (scope, name, server) = (scope.clone(), name.to_owned(), server.clone());
    let proxy = Mutex::new(None);
    Arc::new(move || match server.transport {
        McpTransport::Stdio => stdio(&scope, &name, &server, &proxy, &caller),
        McpTransport::Http => http(&scope, &name, &server),
    })
}

/// The scratch directory of server `name`.
fn scratch(scope: &ProfileScope, name: &str) -> PathBuf {
    scope.dir().join("cache/mcp").join(name)
}

fn stdio(
    scope: &ProfileScope,
    name: &str,
    server: &McpServerConfig,
    proxy_slot: &Mutex<Option<Proxy>>,
    caller: &crate::egress::Caller,
) -> Result<Launch, String> {
    let config = scope.config();
    let scratch = scratch(scope, name);
    std::fs::create_dir_all(&scratch).map_err(|e| format!("{}: {e}", scratch.display()))?;
    let workspace = std::env::current_dir().unwrap_or_else(|_| scope.dir().to_path_buf());
    let fill = |text: &str| super::placeholders::fill(scope, &workspace, text);
    let command_text = fill(&server.command)?;
    let program = program(&command_text);
    let capabilities = baseline(&config.security).union(&declared(server, program.as_deref())?);
    let paths = Paths {
        user_home: std::env::var_os("HOME").map(Into::into).unwrap_or_default(),
        prometheus_home: scope.home().to_path_buf(),
        workspace: workspace.clone(),
        scratch: vec![scratch.clone()],
    };
    // Its network goes through an egress proxy under the profile's egress
    // policy (spec: security-boundaries, Egress enforcement for child
    // processes).
    let hosts = Arc::new(crate::egress::host_policy(
        &config.security.egress,
        &crate::egress::Overrides::default(),
    ));
    let requester = format!("mcp:{name}");
    let observer = crate::egress::attributed(scope, Arc::clone(caller));
    let proxy = crate::egress::child_proxy(scope, &capabilities, hosts, &requester, observer)
        .map_err(|e| e.message().to_owned())?;
    let mut policy = Policy::from_set(&capabilities, &paths);
    if let Some(proxy) = &proxy {
        policy = crate::egress::through(policy, proxy);
    }
    let sandbox =
        Sandbox::new(policy, &config.security.sandbox).map_err(|e| e.message().to_owned())?;
    let mut command = sandbox
        .command(program.as_deref().unwrap_or(Path::new(&command_text)))
        .map_err(|e| e.message().to_owned())?;
    let args = server
        .args
        .iter()
        .map(|a| fill(a))
        .collect::<Result<Vec<_>, _>>()?;
    command.args(&args).current_dir(&workspace).env_clear();
    for var in INHERITED {
        if let Some(value) = std::env::var_os(var) {
            command.env(var, value);
        }
    }
    command.env("TMPDIR", &scratch);
    if let Some(proxy) = &proxy {
        for var in pmt_net::egress::PROXY_VARS {
            command.env(var, proxy.url());
        }
    }
    *proxy_slot.lock().unwrap_or_else(PoisonError::into_inner) = proxy;
    for (key, value) in &server.env {
        command.env(key, fill(value)?);
    }
    let only = pmt_config::secrets::surface::SurfaceOnly::of(config);
    for secret in &server.env_secrets {
        only.check(secret).map_err(|e| e.message().to_owned())?;
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

fn http(scope: &ProfileScope, name: &str, server: &McpServerConfig) -> Result<Launch, String> {
    let network = &scope.config().network;
    let policy = OutboundPolicy {
        force_ipv4: network.force_ipv4,
        extra_ca_file: network.extra_ca_file.as_ref().map(Into::into),
        proxy: network.proxy.clone(),
    };
    let client = match super::tls::identity(scope, name, server)? {
        Some(identity) => pmt_net::client_with_identity(&policy, identity),
        None => pmt_net::client(&policy),
    }
    .map_err(|e| e.message().to_owned())?;
    let workspace = std::env::current_dir().unwrap_or_else(|_| scope.dir().to_path_buf());
    let fill = |text: &str| super::placeholders::fill(scope, &workspace, text);
    let mut headers = server
        .headers
        .iter()
        .map(|(k, v)| Ok((k.clone(), fill(v)?)))
        .collect::<Result<Vec<_>, String>>()?;
    if let Some(identity) = super::tls::identity_header(scope, name, server) {
        headers.push(identity);
    }
    let bearer = match server.auth {
        Some(McpAuth::Oauth) => Some(super::oauth::bearer(scope, name, server)?),
        _ if server.auth_secret.is_empty() => None,
        _ => {
            pmt_config::secrets::surface::SurfaceOnly::of(scope.config())
                .check(&server.auth_secret)
                .map_err(|e| e.message().to_owned())?;
            let token = scope
                .secrets()
                .resolve(&server.auth_secret)
                .map_err(|e| e.message().to_owned())?;
            Some(token.expose().to_owned())
        }
    };
    Ok(Launch::Http {
        url: fill(&server.url)?,
        client,
        bearer,
        headers,
    })
}
