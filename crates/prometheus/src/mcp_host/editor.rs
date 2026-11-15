//! Editor MCP servers (spec: acp-adapter, Editor MCP servers belong to
//! the daemon session): the servers an editor hands an ACP session are run
//! by the daemon for that session alone, launched like configured servers
//! (the profile's sandbox and egress policy, no inherited secrets). Each
//! session with editor servers has its own server set; its tools join the
//! session's registry on top of the profile's, from the next turn after a
//! change.
//!
//! Editor definitions are data from the client: `${NAME}` placeholders
//! (which a configured server fills from the secrets store) are refused,
//! so no editor can make the daemon hand it a stored secret, and the
//! legacy `sse` transport, which the MCP client does not speak, is
//! skipped with a warning.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_config::schema::{McpServerConfig, McpTransport};
use pmt_mcp::Manager;
use pmt_rpc::contracts::EditorMcpServer;

/// How long a turn waits for newly bound servers to connect.
const CONNECT_WAIT: Duration = Duration::from_secs(10);

/// One session's editor servers.
struct Bound {
    servers: Vec<EditorMcpServer>,
    manager: Arc<Manager>,
    /// Counts the session's binds, so its tools follow a change.
    generation: u64,
}

static BOUND: LazyLock<Mutex<HashMap<String, Bound>>> = LazyLock::new(Mutex::default);

fn bound() -> std::sync::MutexGuard<'static, HashMap<String, Bound>> {
    BOUND.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The configuration of editor server `server`, or why it is refused.
fn config_of(server: &EditorMcpServer) -> Result<McpServerConfig, String> {
    let texts = server
        .command
        .iter()
        .chain(&server.args)
        .chain(server.url.iter())
        .chain(server.env.iter().map(|v| &v.value))
        .chain(server.headers.iter().map(|v| &v.value));
    if texts.into_iter().any(|t| t.contains("${")) {
        return Err("its definition holds a ${...} placeholder".to_owned());
    }
    let pairs = |list: &[pmt_rpc::contracts::NameValue]| -> BTreeMap<String, String> {
        list.iter()
            .map(|v| (v.name.clone(), v.value.clone()))
            .collect()
    };
    let transport = match server.transport.as_str() {
        "stdio" => McpTransport::Stdio,
        "http" => McpTransport::Http,
        other => return Err(format!("transport '{other}' is not supported")),
    };
    Ok(McpServerConfig {
        transport,
        command: server.command.clone().unwrap_or_default(),
        args: server.args.clone(),
        env: pairs(&server.env),
        url: server.url.clone().unwrap_or_default(),
        headers: pairs(&server.headers),
        lazy: true,
        ..McpServerConfig::default()
    })
}

/// Binds `servers` to `session` (an empty list unbinds); the old set
/// stops. Nothing changes when the list is the one already bound.
pub fn bind(scope: &ProfileScope, session: &str, servers: &[EditorMcpServer]) {
    let mut bound = bound();
    if bound.get(session).is_some_and(|b| b.servers == servers) {
        return;
    }
    let generation = bound.get(session).map_or(0, |b| b.generation) + 1;
    if let Some(old) = bound.remove(session) {
        old.manager.stop();
    }
    if servers.is_empty() {
        return;
    }
    let host: Arc<dyn pmt_mcp::ClientHost> = Arc::new(super::sessions::SessionsHost {
        profile_dir: scope.dir().to_path_buf(),
    });
    let mut made = Vec::new();
    for server in servers {
        let built = config_of(server).and_then(|config| {
            let settings = super::settings(scope, &server.name, &config)?;
            Ok(super::new_server(
                scope,
                &server.name,
                &config,
                settings,
                &host,
            ))
        });
        match built {
            Ok(server) => made.push(server),
            Err(why) => tracing::warn!("editor MCP server '{}' skipped: {why}", server.name),
        }
    }
    let manager = Arc::new(Manager::from_servers(made, 4));
    bound.insert(
        session.to_owned(),
        Bound {
            servers: servers.to_vec(),
            manager,
            generation,
        },
    );
}

/// Forgets `session`'s servers (the session closed); they stop.
pub fn unbind(session: &str) {
    if let Some(old) = bound().remove(session) {
        old.manager.stop();
    }
}

/// `session`'s editor servers and the generation of their binding.
#[must_use]
pub fn manager(session: &str) -> Option<(Arc<Manager>, u64)> {
    bound()
        .get(session)
        .map(|b| (Arc::clone(&b.manager), b.generation))
}

/// The generation of `session`'s binding (0: never bound).
#[must_use]
pub fn generation(session: &str) -> u64 {
    bound().get(session).map_or(0, |b| b.generation)
}

/// The toolsets of `session`'s editor servers (opted in when bound).
#[must_use]
pub fn toolsets(session: &str) -> Vec<String> {
    manager(session).map_or_else(Vec::new, |(manager, _)| {
        manager
            .servers()
            .iter()
            .map(|s| pmt_mcp::naming::toolset(s.name()))
            .collect()
    })
}

/// Connects `session`'s editor servers (their tools are listed only
/// once connected), waiting a bounded time.
pub async fn connect(session: &str) {
    let Some((manager, _)) = manager(session) else {
        return;
    };
    for (server, outcome) in manager.discover_within(Some(CONNECT_WAIT)).await {
        if let Err(reason) = outcome {
            tracing::warn!(server = %server, "editor MCP server unavailable: {reason}");
        }
    }
}

#[cfg(test)]
mod tests {
    use pmt_rpc::contracts::{EditorMcpServer, NameValue};

    use super::config_of;

    fn server(transport: &str) -> EditorMcpServer {
        EditorMcpServer {
            name: "db".into(),
            transport: transport.into(),
            command: Some("db-mcp".into()),
            args: vec!["--ro".into()],
            env: vec![NameValue {
                name: "DB".into(),
                value: "local".into(),
            }],
            url: None,
            headers: Vec::new(),
        }
    }

    #[test]
    fn an_editor_server_becomes_a_lazy_stdio_server() {
        let config = config_of(&server("stdio")).unwrap();
        assert_eq!(config.command, "db-mcp");
        assert_eq!(config.env["DB"], "local");
        assert!(config.lazy);
    }

    #[test]
    fn placeholders_and_unknown_transports_are_refused() {
        let mut leaking = server("stdio");
        leaking.args.push("${OPENAI_API_KEY}".into());
        assert!(config_of(&leaking).unwrap_err().contains("placeholder"));
        assert!(config_of(&server("sse")).unwrap_err().contains("sse"));
    }
}
