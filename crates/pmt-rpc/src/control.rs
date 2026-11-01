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
    /// A graceful drain and exit; `restart` marks the daemon retiring so
    /// it refuses new work while it drains.
    Stop {
        /// The caller starts a new daemon once this one exits.
        #[serde(default)]
        restart: bool,
    },
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
    /// Each served profile's configuration: reloads, the error of an
    /// invalid edit it ignored, keys waiting for a restart.
    #[serde(default)]
    pub config: Vec<ProfileConfig>,
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
    /// Set when the request failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ControlError>,
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
