//! The local control channel (spec: workspace-architecture, Daemon
//! discovery file and local control channel; Ticket-authenticated loopback
//! attachment): a Unix domain socket (a named pipe on Windows) only the
//! owning user can open, carrying one JSON request line and one JSON
//! response line per connection. Clients get their single-use RPC tickets
//! here; `prometheus daemon stop|restart|status` use it too.

use std::time::Duration;

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader};

/// What a ticket is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Purpose {
    /// A CLI, TUI or desktop client.
    Interactive,
    /// The HTTP surfaces (dashboard, API server).
    Http,
    /// Administration across profiles; the only purpose granting several.
    ProfileAdmin,
    /// The ACP adapter.
    Acp,
    /// `prometheus eval`.
    Eval,
}

impl Purpose {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Http => "http",
            Self::ProfileAdmin => "profile-admin",
            Self::Acp => "acp",
            Self::Eval => "eval",
        }
    }
}

/// One control request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ControlRequest {
    /// A single-use RPC ticket.
    Ticket {
        /// What the connection is for.
        purpose: Purpose,
        /// The profiles it grants: exactly one unless `profile-admin`.
        profiles: Vec<String>,
        /// The requesting client's pid, named to a second client refused a
        /// session this one owns (`session <id> is open in another client
        /// (pid <n>)`). Only the owning user can reach this channel.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pid: Option<u32>,
    },
    /// The daemon's process state.
    Status,
    /// Asks a connected adapter for the id of a role name (the hint of
    /// an `allow_roles` entry that names a role instead of its id).
    ResolveRole {
        /// The owning profile.
        profile: String,
        /// The platform.
        platform: String,
        /// The role name.
        name: String,
    },
    /// Pauses or resumes one messaging adapter of a profile (`prometheus
    /// gateway pause|resume <platform>`); answers the new status.
    GatewayPlatform {
        /// The owning profile.
        profile: String,
        /// The platform.
        platform: String,
        /// Pause (`true`) or resume.
        pause: bool,
    },
    /// Starts the messaging adapters served profiles enable that are not
    /// running (`prometheus gateway reload`); answers the new status.
    GatewayReload,
    /// Queues one message in a profile's outbox without an agent turn
    /// (`prometheus gateway send`); answers the stored entry.
    GatewaySend {
        /// The owning profile.
        profile: String,
        /// `platform`, `platform:chat_id[:thread_id]` or `platform:#name`.
        target: String,
        /// The text (`MEDIA:` directives already taken out).
        text: String,
        /// Files sent with it.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<SendAttachment>,
    },
    /// Hands a session over to a chat of its profile (`/handoff` in the CLI
    /// or TUI); answers where it went.
    GatewayHandoff {
        /// The owning profile.
        profile: String,
        /// The session.
        session: String,
        /// `platform[:chat_id[:thread_id]]` (a bare platform is its home
        /// channel).
        target: String,
    },
    /// Replaces a profile's live plugin set (`prometheus plugins reload
    /// [<id>]`); answers what is active now and what waits.
    PluginsReload {
        /// The owning profile.
        profile: String,
        /// The plugin the report is about (every plugin reloads).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plugin: Option<String>,
    },
    /// Writes a dump of every thread and task to
    /// `<home>/logs/task-dump-<UTC>.txt` without stopping (`prometheus
    /// debug dump-tasks`); answers its path.
    DumpTasks,
    /// Opens, reports or stops the web dashboard's listener (`prometheus
    /// dashboard`).
    Dashboard {
        /// What to do.
        action: DashboardAction,
    },
    /// A graceful drain and exit; `restart` marks the daemon retiring so
    /// it refuses new work while it drains.
    Stop {
        /// The caller starts a new daemon once this one exits.
        #[serde(default)]
        restart: bool,
    },
}

/// A `dashboard` control request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case", deny_unknown_fields)]
pub enum DashboardAction {
    /// Starts the listener unless it runs (`host`/`port` override
    /// `daemon.toml` for the daemon's lifetime) and mints a one-time
    /// login link.
    Open {
        /// `--host`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<String>,
        /// `--port`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        /// The page the login link lands on (`/app` for the webapp).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next: Option<String>,
    },
    /// The running listener's URL.
    Status,
    /// Stops the listener; the daemon keeps running.
    Stop,
    /// Mints a one-use pairing code (10 minutes) for a device.
    Pair {
        /// The device's name.
        name: String,
        /// `driver` or `observer`.
        role: String,
    },
    /// The paired devices.
    Devices,
    /// Revokes a paired device, closing its sockets with 4401.
    Revoke {
        /// The device's name.
        name: String,
    },
    /// Signs every browser out (`prometheus webapp --sign-out`); paired
    /// devices stay signed in.
    SignOut,
}

/// A paired device, as `Devices` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceEntry {
    /// Its name.
    pub name: String,
    /// `driver` or `observer`.
    pub role: String,
    /// The profiles it may use (empty: every served profile).
    #[serde(default)]
    pub profiles: Vec<String>,
    /// When it was paired (RFC 3339).
    pub paired_at: String,
    /// When its credential last rotated (RFC 3339).
    pub rotated_at: String,
}

/// The dashboard listener, as a `dashboard` control request reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardState {
    /// The dashboard's URL (`http://127.0.0.1:9120/`).
    pub url: String,
    /// A one-time login URL (`Open` only); single use, 60 s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_url: Option<String>,
    /// `Open` started the listener (it was not running).
    #[serde(default)]
    pub started: bool,
    /// `Pair`: the one-use pairing code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pairing_code: Option<String>,
    /// `Devices`: the paired devices.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<DeviceEntry>,
    /// `SignOut`: how many browser sessions ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_out: Option<u64>,
}

impl DashboardState {
    /// A state naming only `url`.
    #[must_use]
    pub fn at(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            login_url: None,
            started: false,
            pairing_code: None,
            devices: Vec::new(),
            signed_out: None,
        }
    }
}

/// A file sent with a `gateway send` message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendAttachment {
    /// Absolute path the daemon reads.
    pub path: String,
    /// Sent as a document (images uncompressed).
    #[serde(default)]
    pub as_document: bool,
}

/// A message `gateway send` queued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Queued {
    /// The outbox entry id.
    pub entry: i64,
    /// Where it goes (`platform:chat_id[:thread_id]`).
    pub destination: String,
}

/// Where `gateway_handoff` moved a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandedOff {
    /// The platform.
    pub platform: String,
    /// `platform:chat_id[:thread_id]`.
    pub destination: String,
}

/// A granted ticket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ticket {
    /// The secret; it travels only as the `prometheus-ticket.<ticket>`
    /// WebSocket subprotocol.
    pub ticket: String,
    /// Seconds until it expires.
    pub expires_in_secs: u64,
}

/// What `daemon status` prints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    /// The daemon's pid.
    pub pid: u32,
    /// Seconds since it started.
    pub uptime_secs: u64,
    /// Its semantic version.
    pub version: String,
    /// Its build id.
    pub build_id: String,
    /// Random per start.
    pub instance_id: String,
    /// Its RPC URL.
    pub rpc_url: String,
    /// The protocol, `<major>.<minor>`.
    pub rpc_protocol: String,
    /// The profiles it serves.
    pub profiles: Vec<String>,
    /// Open client connections.
    pub connections: usize,
    /// Sessions loaded in the daemon.
    pub sessions: usize,
    /// Turns running now.
    pub active_turns: usize,
    /// It is draining to stop or restart.
    pub retiring: bool,
    /// A supervisor (container init, systemd, launchd) starts it again
    /// after a restart exit (75); otherwise a restart must start it.
    #[serde(default)]
    pub supervised: bool,
    /// The executable it started from (an update hands over only the
    /// daemon of the binary it replaced).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<std::path::PathBuf>,
    /// Each served profile's configuration: reloads, the error of an
    /// invalid edit it ignored, keys waiting for a restart.
    #[serde(default)]
    pub config: Vec<ProfileConfig>,
    /// Each messaging adapter's state (spec: messaging-gateway, One gateway
    /// daemon per home; platform-adapters, Adapter state reporting).
    #[serde(default)]
    pub adapters: Vec<AdapterReport>,
    /// Every component's health (spec: observability, Component health
    /// model).
    #[serde(default)]
    pub health: Vec<ComponentReport>,
    /// Subagents running now.
    #[serde(default)]
    pub subagents: usize,
    /// The OTLP exporter's posture and counters, when export is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export: Option<ExportReport>,
}

/// One component in `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentReport {
    /// `store`, `scheduler`, `adapter:<platform>`, `mcp:<server>`...
    pub component: String,
    /// The owning profile; `None` for daemon-wide components.
    #[serde(default)]
    pub profile: Option<String>,
    /// `ok`, `degraded` or `down`.
    pub state: String,
    /// Why.
    pub reason: String,
    /// Since when (RFC 3339).
    pub since: String,
}

/// The running OTLP exporter (`status --exports`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportReport {
    /// The collector's host (never credentials).
    pub host: String,
    /// `http/protobuf` or `grpc`.
    pub protocol: String,
    /// Message content, tool arguments and output are exported.
    pub capture_content: bool,
    /// `traceparent` goes to external hosts.
    pub propagate_external: bool,
    /// Items waiting.
    pub queued: u64,
    /// Items dropped since start.
    pub dropped: u64,
    /// Items exported since start.
    pub exported: u64,
    /// The last successful export (RFC 3339).
    #[serde(default)]
    pub last_success: Option<String>,
    /// The last failure.
    #[serde(default)]
    pub last_error: Option<String>,
}

/// One messaging adapter in `daemon status` and `gateway status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterReport {
    /// The owning profile.
    pub profile: String,
    /// Platform name.
    pub platform: String,
    /// `disabled`, `connecting`, `connected`, `degraded`, `retrying` or
    /// `fatal`.
    pub state: String,
    /// Error code of a `degraded`, `retrying` or `fatal` adapter.
    pub code: Option<String>,
    /// What happened.
    pub message: Option<String>,
    /// Outside `connected` longer than `gateway.reconnect_attention_secs`.
    #[serde(default)]
    pub attention: bool,
    /// Paused by an admin (`/platform pause`, `gateway pause`).
    #[serde(default)]
    pub paused: bool,
    /// The capability descriptor the adapter loaded with (`gateway
    /// platforms`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub descriptor: Option<serde_json::Value>,
}

/// A served profile's configuration state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileConfig {
    /// The profile.
    pub profile: String,
    /// Changed files applied since the daemon started.
    pub reloads: u64,
    /// Why the current `config.toml` was not applied (the daemon serves
    /// the previous configuration meanwhile).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Changed `restart` keys that apply after a daemon restart.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_restart: Vec<String>,
}

/// An error answered on the control channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlError {
    /// The dotted error code.
    pub code: String,
    /// One line.
    pub message: String,
}

/// One control response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlResponse {
    /// For `ticket`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<Ticket>,
    /// For `status`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<DaemonStatus>,
    /// For `gateway_send`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued: Option<Queued>,
    /// For `gateway_handoff`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<HandedOff>,
    /// Set when the request failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ControlError>,
    /// `ResolveRole`: the role's id, when a connected adapter knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<String>,
    /// `PluginsReload`: the reload report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reloaded: Option<String>,
    /// `Dashboard`: the listener.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dashboard: Option<DashboardState>,
    /// `DumpTasks`: the dump's path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dumped: Option<String>,
}

/// How long a control exchange may take.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// Sends `request` over `stream` and reads the response line.
///
/// # Errors
///
/// `rpc.connection` on I/O failure, a timeout or a malformed response.
pub async fn exchange<S>(stream: S, request: &ControlRequest) -> Result<ControlResponse, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let failed =
        |what: String| Error::new(&codes::RPC_CONNECTION, format!("control channel: {what}"));
    let work = async {
        let mut stream = BufReader::new(stream);
        let mut line = serde_json::to_string(request).unwrap_or_default();
        line.push('\n');
        stream.get_mut().write_all(line.as_bytes()).await?;
        stream.get_mut().flush().await?;
        let mut answer = String::new();
        stream.read_line(&mut answer).await?;
        Ok::<_, std::io::Error>(answer)
    };
    let answer = tokio::time::timeout(TIMEOUT, work)
        .await
        .map_err(|_| failed("no answer in time".into()))?
        .map_err(|e| failed(e.to_string()))?;
    serde_json::from_str(&answer).map_err(|e| failed(format!("malformed response: {e}")))
}

/// Sends `request` to the control channel at `control`.
///
/// # Errors
///
/// `rpc.connection` when the channel cannot be opened or answers badly.
pub async fn call(control: &str, request: &ControlRequest) -> Result<ControlResponse, Error> {
    let stream = connect(control).await.map_err(|e| {
        Error::new(
            &codes::RPC_CONNECTION,
            format!("cannot open the daemon control channel {control}: {e}"),
        )
    })?;
    exchange(stream, request).await
}

#[cfg(unix)]
async fn connect(control: &str) -> std::io::Result<tokio::net::UnixStream> {
    tokio::net::UnixStream::connect(control).await
}

/// Opens the pipe, waiting while every instance is busy with another
/// client (`ERROR_PIPE_BUSY`).
#[cfg(windows)]
async fn connect(
    control: &str,
) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    const ERROR_PIPE_BUSY: i32 = 231;
    let mut attempts = 0;
    loop {
        match tokio::net::windows::named_pipe::ClientOptions::new().open(control) {
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && attempts < 100 => {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            other => return other,
        }
    }
}

/// The `ticket` answer of `response`.
///
/// # Errors
///
/// The control error the daemon answered, or `rpc.ticket` for an empty
/// answer.
pub fn ticket_of(response: ControlResponse) -> Result<Ticket, Error> {
    if let Some(err) = response.error {
        return Err(Error::new(&codes::RPC_TICKET, err.message));
    }
    response
        .ticket
        .ok_or_else(|| Error::new(&codes::RPC_TICKET, "the daemon granted no ticket"))
}

impl AdapterReport {
    /// `platform: state (code): message [paused] [needs attention]`.
    #[must_use]
    pub fn line(&self) -> String {
        use std::fmt::Write as _;

        let mut line = format!("{}: {}", self.platform, self.state);
        if let Some(code) = &self.code {
            let _ = write!(line, " ({code})");
        }
        if let Some(message) = &self.message {
            let _ = write!(line, ": {message}");
        }
        if self.paused {
            line.push_str(" [paused]");
        }
        if self.attention {
            line.push_str(" [needs attention]");
        }
        line
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{ControlRequest, Purpose};

    #[test]
    fn requests_have_their_wire_shape() {
        let request = ControlRequest::Ticket {
            purpose: Purpose::ProfileAdmin,
            profiles: vec!["work".into()],
            pid: None,
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({"op": "ticket", "purpose": "profile-admin", "profiles": ["work"]})
        );
        let stop: ControlRequest = serde_json::from_value(json!({"op": "stop"})).unwrap();
        assert_eq!(stop, ControlRequest::Stop { restart: false });
        assert!(
            serde_json::from_value::<ControlRequest>(json!({"op": "ticket", "purpose": "root"}))
                .is_err()
        );
    }
}
