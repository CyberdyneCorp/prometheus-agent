//! Embed Prometheus in a Rust program (spec: workspace-architecture,
//! Embeddable client SDKs): sessions, prompts and their events over the
//! daemon's versioned RPC protocol, the one every client speaks (its
//! types are `pmt_rpc`'s, from which `schema/rpc.json` is generated).
//!
//! ```no_run
//! # async fn demo() -> Result<(), pmt_core::error::Error> {
//! use pmt_sdk::{Client, Connect, Create};
//!
//! let client = Client::connect(&Connect::default()).await?;
//! let session = client.create(&Create::default().toolsets(&["file"])).await?;
//! let turn = session.prompt("list files", |event| println!("{}", event.kind)).await?;
//! println!("{:?}", turn.usage);
//! # Ok(()) }
//! ```
//!
//! [`Client::connect`] attaches to the home's daemon through the ticketed
//! loopback flow (starting it with `prometheus daemon ensure` when none
//! runs) and refuses a daemon whose protocol major is not in
//! [`SUPPORTED_PROTOCOL_MAJORS`] (`rpc.protocol_mismatch`, the `-32008`
//! handshake error) before sending any request. An embedded session is an
//! ordinary client session: the same capability grants, approvals and
//! budgets apply. One client runs one prompt at a time.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_core::error::{Error, codes};
use pmt_rpc::client::{Client as Rpc, Inbox, ServerCall, error_of};
use pmt_rpc::contracts::{
    ClientCapabilities, ClientKind, HelloParams, InterruptParams, PromptSubmit, SessionCreate,
    SessionCreateParams, SessionInterrupt, SessionRef, SessionSetup, SessionTranscript,
    SubmitParams, TranscriptMessage, Version,
};
use pmt_rpc::control::Purpose;
use pmt_rpc::discovery::{Discovery, Endpoint, discover};
use serde_json::{Value, json};
use tokio::sync::Mutex;

/// The protocol majors this release speaks.
pub const SUPPORTED_PROTOCOL_MAJORS: &[u16] = &[1];
/// The minor it was written against.
pub const PROTOCOL_MINOR: u16 = 0;

/// One event of a turn: a trajectory event (`tool.call`, `tool.result`,
/// `assistant.message`...).
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// Its type.
    pub kind: String,
    /// Its fields.
    pub data: Value,
}

/// How a turn ended (`turn.finished`).
#[derive(Debug, Clone, PartialEq)]
pub struct Turn {
    /// `completed`, `interrupted_by_user`...
    pub exit_reason: String,
    /// The final answer.
    pub final_response: Option<String>,
    /// The turn's token and cost totals.
    pub usage: Option<Value>,
    /// The error that ended the turn.
    pub error: Option<Value>,
}

/// Answers an `approval.request`'s params with a decision (`once`,
/// `session`, `always`, `deny`).
pub type Approver = Arc<dyn Fn(&Value) -> String + Send + Sync>;
/// Answers a `clarify.request`'s params with one answer per question.
pub type Clarifier = Arc<dyn Fn(&Value) -> Vec<Value> + Send + Sync>;

/// How to reach the daemon.
#[derive(Clone, Default)]
pub struct Connect {
    /// The Prometheus home (default: the user's).
    pub home: Option<PathBuf>,
    /// The profile (default: `default`).
    pub profile: Option<String>,
    /// The `prometheus` binary that starts a daemon (default: on `PATH`).
    pub binary: Option<PathBuf>,
    /// Answers approvals; without one the daemon's own fallback applies
    /// (unanswered requests time out as denials).
    pub approver: Option<Approver>,
    /// Answers clarifying questions.
    pub clarifier: Option<Clarifier>,
}

impl std::fmt::Debug for Connect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connect")
            .field("home", &self.home)
            .field("profile", &self.profile)
            .field("binary", &self.binary)
            .finish_non_exhaustive()
    }
}

/// `sessions.create` options.
#[derive(Debug, Clone, Default)]
pub struct Create {
    toolsets: Option<Vec<String>>,
    model: Option<String>,
    overlay: Option<String>,
    ephemeral: bool,
}

impl Create {
    /// Only these toolsets.
    #[must_use]
    pub fn toolsets(mut self, toolsets: &[&str]) -> Self {
        self.toolsets = Some(toolsets.iter().map(|&t| t.to_owned()).collect());
        self
    }

    /// This model (`[<provider>/]<model>` or an alias).
    #[must_use]
    pub fn model(mut self, model: &str) -> Self {
        self.model = Some(model.to_owned());
        self
    }

    /// Text sent as a last system block with every model call of this
    /// session only, never stored.
    #[must_use]
    pub fn system_prompt_overlay(mut self, text: &str) -> Self {
        self.overlay = Some(text.to_owned());
        self
    }

    /// Keep the transcript out of the session store and trajectories.
    #[must_use]
    pub fn ephemeral(mut self, ephemeral: bool) -> Self {
        self.ephemeral = ephemeral;
        self
    }

    fn setup(&self) -> SessionSetup {
        let mut setup = SessionSetup {
            model: self.model.clone(),
            system_prompt_overlay: self.overlay.clone(),
            ephemeral: self.ephemeral,
            ..SessionSetup::default()
        };
        if let Some(toolsets) = &self.toolsets {
            let list = serde_json::to_string(toolsets).expect("strings serialize");
            setup.set.push(format!("tools.enabled_toolsets={list}"));
        }
        setup
    }
}

/// The major of `<major>.<minor>` when this SDK speaks it.
///
/// # Errors
///
/// `rpc.protocol_mismatch` (exit 76).
pub fn check_protocol(version: &str) -> Result<u16, Error> {
    let major = version
        .split('.')
        .next()
        .and_then(|m| m.parse::<u16>().ok());
    match major {
        Some(major) if SUPPORTED_PROTOCOL_MAJORS.contains(&major) => Ok(major),
        _ => Err(Error::new(
            &codes::RPC_PROTOCOL_MISMATCH,
            format!(
                "the daemon speaks protocol {version}; this SDK supports major {SUPPORTED_PROTOCOL_MAJORS:?}"
            ),
        )
        .with_hint("use an SDK release that supports the daemon's protocol")),
    }
}

/// A connection to the daemon.
pub struct Client {
    rpc: Rpc,
    inbox: Mutex<Inbox>,
    approver: Option<Approver>,
    clarifier: Option<Clarifier>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").finish_non_exhaustive()
    }
}

impl Client {
    /// Attaches to the daemon `options` name, starting it when none runs.
    ///
    /// # Errors
    ///
    /// No daemon could be started, `rpc.protocol_mismatch` (nothing sent),
    /// ticket and connection errors.
    pub async fn connect(options: &Connect) -> Result<Self, Error> {
        let endpoint = ensure(options).await?;
        let major = check_protocol(&endpoint.rpc_protocol)?;
        let profile = options.profile.clone().unwrap_or_else(|| "default".into());
        let hello = HelloParams {
            client: ClientKind::Cli,
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol: Version {
                major,
                minor: PROTOCOL_MINOR,
            },
            capabilities: ClientCapabilities {
                // Unanswered requests resolve at once without callbacks.
                server_requests: options.approver.is_some() || options.clarifier.is_some(),
                events: Vec::new(),
                features: Vec::new(),
            },
        };
        let (rpc, inbox) =
            Rpc::connect(&endpoint, Purpose::Interactive, vec![profile], &hello).await?;
        check_protocol(&rpc.hello().protocol.to_string())?;
        Ok(Self {
            rpc,
            inbox: Mutex::new(inbox),
            approver: options.approver.clone(),
            clarifier: options.clarifier.clone(),
        })
    }

    /// A new session of the client's profile, attached to this client.
    ///
    /// # Errors
    ///
    /// The daemon refused it.
    pub async fn create(&self, options: &Create) -> Result<Session<'_>, Error> {
        let params = SessionCreateParams {
            setup: options.setup(),
            ..SessionCreateParams::default()
        };
        let created = self
            .rpc
            .call_typed::<SessionCreate>(&params)
            .await
            .map_err(|e| error_of(&e))?;
        Ok(Session {
            client: self,
            id: created.session_id,
        })
    }

    fn answer(&self, call: &ServerCall) {
        let result = match (call.method.as_str(), &self.approver, &self.clarifier) {
            ("approval.request", Some(approve), _) => {
                Ok(json!({"decision": approve(&call.params)}))
            }
            ("clarify.request", _, Some(clarify)) => Ok(json!({"answers": clarify(&call.params)})),
            _ => Err(pmt_rpc::frame::RpcError::unknown_method(&call.method)),
        };
        self.rpc.respond(&call.id, result);
    }
}

/// One session.
#[derive(Debug)]
pub struct Session<'a> {
    client: &'a Client,
    /// Its id.
    pub id: String,
}

impl Session<'_> {
    /// Runs one turn, passing each of its events to `on_event`; returns
    /// how it ended. Approval and clarify requests go to the client's
    /// callbacks meanwhile.
    ///
    /// # Errors
    ///
    /// The daemon refused the prompt or the connection closed.
    pub async fn prompt(&self, text: &str, on_event: impl FnMut(&Event)) -> Result<Turn, Error> {
        self.prompt_with(text, &[], on_event).await
    }

    /// [`Session::prompt`] with `attachments`: files on the daemon's
    /// machine, kept with the session.
    ///
    /// # Errors
    ///
    /// As [`Session::prompt`].
    pub async fn prompt_with(
        &self,
        text: &str,
        attachments: &[PathBuf],
        mut on_event: impl FnMut(&Event),
    ) -> Result<Turn, Error> {
        let params = SubmitParams {
            session_id: self.id.clone(),
            text: text.to_owned(),
            images: Vec::new(),
            files: attachments
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            run_budget_secs: None,
            input_id: None,
            author: None,
        };
        let admitted = self
            .client
            .rpc
            .call_typed::<PromptSubmit>(&params)
            .await
            .map_err(|e| error_of(&e))?;
        let mut inbox = self.client.inbox.lock().await;
        let Inbox { events, requests } = &mut *inbox;
        loop {
            tokio::select! {
                call = requests.recv() => self.client.answer(&call.ok_or_else(closed)?),
                envelope = events.recv() => {
                    let envelope = envelope.ok_or_else(closed)?;
                    if envelope.session_id.as_deref() != Some(self.id.as_str()) {
                        continue;
                    }
                    match envelope.event.as_str() {
                        "session.event" => on_event(&Event {
                            kind: envelope.data["type"].as_str().unwrap_or_default().to_owned(),
                            data: envelope.data,
                        }),
                        "turn.finished" if envelope.data["turn_id"] == admitted.turn_id.as_str() => {
                            return Ok(finished(&envelope.data));
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// Stops the running turn at its next safe point.
    ///
    /// # Errors
    ///
    /// The daemon refused it.
    pub async fn interrupt(&self) -> Result<(), Error> {
        let params = InterruptParams {
            session_id: self.id.clone(),
            turn_id: None,
        };
        self.client
            .rpc
            .call_typed::<SessionInterrupt>(&params)
            .await
            .map(drop)
            .map_err(|e| error_of(&e))
    }

    /// The session's messages so far (multi-turn use).
    ///
    /// # Errors
    ///
    /// The daemon refused it.
    pub async fn history(&self) -> Result<Vec<TranscriptMessage>, Error> {
        let params = SessionRef {
            session_id: self.id.clone(),
        };
        self.client
            .rpc
            .call_typed::<SessionTranscript>(&params)
            .await
            .map(|t| t.messages)
            .map_err(|e| error_of(&e))
    }
}

fn finished(data: &Value) -> Turn {
    let field = |key: &str| Some(data[key].clone()).filter(|v| !v.is_null());
    Turn {
        exit_reason: data["exit_reason"].as_str().unwrap_or_default().to_owned(),
        final_response: data["final_response"].as_str().map(str::to_owned),
        usage: field("usage"),
        error: field("error"),
    }
}

fn closed() -> Error {
    Error::new(&codes::RPC_CONNECTION, "the daemon closed the connection")
}

/// The endpoint of the daemon, started with `prometheus daemon ensure`
/// when none runs.
async fn ensure(options: &Connect) -> Result<Endpoint, Error> {
    if let Some(home) = &options.home
        && let Ok(Discovery::Running(record)) = discover(home)
    {
        return Ok(record.endpoint);
    }
    let binary = options
        .binary
        .as_deref()
        .unwrap_or_else(|| Path::new("prometheus"));
    let mut command = tokio::process::Command::new(binary);
    if let Some(home) = &options.home {
        command.arg("--home").arg(home);
    }
    if let Some(profile) = &options.profile {
        command.args(["--profile", profile]);
    }
    let out = command
        .args(["--json", "daemon", "ensure"])
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| {
            Error::new(
                &codes::DAEMON_START_FAILED,
                format!("cannot run {}: {e}", binary.display()),
            )
        })?;
    let ensured: Value = serde_json::from_slice(&out.stdout).map_err(|_| {
        Error::new(
            &codes::DAEMON_START_FAILED,
            String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        )
    })?;
    let text = |key: &str| ensured[key].as_str().unwrap_or_default().to_owned();
    Ok(Endpoint {
        instance_id: text("instance_id"),
        build_id: text("build_id"),
        rpc_url: text("rpc_url"),
        control: text("control"),
        rpc_protocol: text("rpc_protocol"),
        started_at: String::new(),
        health_url: None,
        features: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_supported_majors_pass() {
        assert_eq!(check_protocol("1.7").unwrap(), 1);
        let err = check_protocol("2.0").unwrap_err();
        assert_eq!(err.exit_code(), 76);
        assert!(check_protocol("garbage").is_err());
    }

    #[test]
    fn toolsets_become_a_session_override() {
        let setup = Create::default()
            .toolsets(&["file"])
            .system_prompt_overlay("be brief")
            .ephemeral(true)
            .setup();
        assert_eq!(setup.set, ["tools.enabled_toolsets=[\"file\"]"]);
        assert_eq!(setup.system_prompt_overlay.as_deref(), Some("be brief"));
        assert!(setup.ephemeral);
    }
}
