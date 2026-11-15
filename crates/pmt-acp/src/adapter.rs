//! The adapter's core: the stdio server loop, request dispatch, the daemon
//! connection and the pump that turns its events into session updates
//! (spec: acp-adapter, ACP process is a daemon viewer; Liveness probes are
//! quiet; Daemon restart yields an unknown outcome, never a resend).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use pmt_rpc::client::{Client, Inbox};
use pmt_rpc::contracts::{
    CommandEntry, EDITOR_FS, EDITOR_TERMINAL, Envelope, SESSION_SETTINGS, SessionSettings,
    TurnFinished,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, BufReader};
use tokio::sync::oneshot;

use crate::content::Fetcher;
use crate::frames::{AcpError, Incoming, METHOD_NOT_FOUND, Out, parse, write_frames};
use crate::kept::KeptPrompts;
use crate::link::{Conn, Connector, PROFILE_MISMATCH};
use crate::project::Projector;

/// Method names editors and supervisors send to check the agent is up:
/// answered `-32601` without a warning.
const PROBES: [&str; 3] = ["ping", "health", "healthcheck"];

/// How many turn ends of unknown admissions are remembered (a turn may
/// end before its admission's acknowledgement is read).
const EARLY_KEPT: usize = 64;

/// What `prometheus acp` was launched with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // independent launch switches
pub struct Launch {
    /// `--model`.
    pub model: Option<String>,
    /// `--provider`.
    pub provider: Option<String>,
    /// `--yolo`.
    pub yolo: bool,
    /// `--safe-mode`.
    pub safe_mode: bool,
    /// `--ignore-user-config`.
    pub ignore_user_config: bool,
}

/// One authentication method an editor may use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthMethod {
    /// Its id.
    pub id: String,
    /// What the editor shows.
    pub name: String,
    /// A longer explanation.
    pub description: String,
    /// For a terminal method: the program and the arguments the editor
    /// runs in a terminal.
    pub terminal: Option<(String, Vec<String>)>,
}

/// The profile's credentials as the editor's sign-in sees them (spec:
/// acp-adapter, Authentication reuses profile credentials).
pub trait Auth: Send + Sync {
    /// The methods `initialize` lists.
    fn methods(&self) -> Vec<AuthMethod>;
    /// Whether method `id` is advertised and its credential resolves now.
    fn resolves(&self, id: &str) -> bool;
}

/// How the adapter runs.
#[derive(Clone)]
pub struct Options {
    /// The launch profile (the only one the daemon ticket grants).
    pub profile: String,
    /// The build version (`prometheus --version`).
    pub version: String,
    /// `--unattended`: the client answers no prompts.
    pub unattended: bool,
    /// The launch flags.
    pub launch: Launch,
    /// The profile's credentials.
    pub auth: Arc<dyn Auth>,
    /// The egress-controlled fetcher of remote images.
    pub fetcher: Arc<dyn Fetcher>,
    /// Fault injection (debug builds' `PMT_TEST_ACP_DROP_ACK`): the first
    /// prompt's acknowledgement is lost with the daemon connection.
    pub drop_first_ack: bool,
}

/// What the editor said it can do (`initialize`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // independent client capabilities
pub(crate) struct ClientCaps {
    pub(crate) read: bool,
    pub(crate) write: bool,
    pub(crate) terminal: bool,
    pub(crate) elicitation: bool,
}

/// How an admission this adapter waits for ended.
#[derive(Debug)]
pub(crate) enum Ended {
    /// Its turn finished.
    Finished(Box<TurnFinished>),
    /// The daemon connection was lost first.
    Lost,
}

/// An admission this process submitted and is waiting for.
#[derive(Debug, Clone, Default)]
pub(crate) struct Admission {
    pub(crate) input_id: String,
    pub(crate) turn_id: Option<String>,
    /// `session/cancel` arrived before the acknowledgement.
    pub(crate) cancel: bool,
    pub(crate) text: String,
}

/// One editor session this process attached.
#[derive(Debug, Default)]
pub(crate) struct Session {
    /// The daemon session it is attached to (a compression tip).
    pub(crate) daemon_id: String,
    pub(crate) projector: Projector,
    /// The registry's `acp`-surface commands.
    pub(crate) commands: Vec<CommandEntry>,
    /// The settings last shown to the editor.
    pub(crate) settings: Option<SessionSettings>,
    /// The title last shown.
    pub(crate) title: Option<String>,
    /// The newest event sequence number seen.
    pub(crate) seen: u64,
    pub(crate) admission: Option<Admission>,
    /// An admission whose outcome a lost connection left unknown.
    pub(crate) unknown: Option<Admission>,
}

/// What the adapter tracks.
#[derive(Debug, Default)]
pub(crate) struct State {
    /// By editor-held id.
    pub(crate) sessions: HashMap<String, Session>,
    /// Daemon session id to editor-held id.
    pub(crate) by_daemon: HashMap<String, String>,
    /// Turns this process waits for, by daemon turn id.
    pub(crate) waiters: HashMap<String, oneshot::Sender<Ended>>,
    /// Turn ends nobody waited for yet.
    pub(crate) early: VecDeque<TurnFinished>,
    pub(crate) kept: KeptPrompts,
}

/// The adapter.
pub(crate) struct Adapter {
    pub(crate) out: Out,
    pub(crate) options: Options,
    connector: Arc<dyn Connector>,
    state: Mutex<State>,
    caps: Mutex<ClientCaps>,
    conn: tokio::sync::Mutex<Option<Conn>>,
    /// The armed fault of [`Options::drop_first_ack`].
    fault: std::sync::atomic::AtomicBool,
}

/// Serves ACP on `input`/`output` until the editor closes `input`.
pub async fn serve<R, W>(input: R, output: W, options: Options, connector: Arc<dyn Connector>)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out, frames) = Out::new();
    let writer = pmt_core::spawn::spawn(write_frames(output, frames));
    let fault = std::sync::atomic::AtomicBool::new(options.drop_first_ack);
    let adapter = Arc::new(Adapter {
        out,
        fault,
        options,
        connector,
        state: Mutex::default(),
        caps: Mutex::default(),
        conn: tokio::sync::Mutex::new(None),
    });
    let mut lines = BufReader::new(input).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if !line.trim().is_empty() {
            adapter.incoming(parse(&line));
        }
    }
    // Leaving only detaches: the daemon keeps every session and turn.
    if let Some(conn) = adapter.conn.lock().await.as_ref() {
        conn.client.close();
    }
    drop(adapter);
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
}

impl Adapter {
    pub(crate) fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the armed fault, once.
    pub(crate) fn take_fault(&self) -> bool {
        self.fault.swap(false, std::sync::atomic::Ordering::AcqRel)
    }

    pub(crate) fn caps(&self) -> ClientCaps {
        *self.caps.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn set_caps(&self, caps: ClientCaps) {
        *self.caps.lock().unwrap_or_else(PoisonError::into_inner) = caps;
    }

    fn incoming(self: &Arc<Self>, frame: Incoming) {
        match frame {
            Incoming::Request { id, method, params } => {
                let me = Arc::clone(self);
                pmt_core::spawn::spawn(async move { me.request(id, &method, params).await });
            }
            Incoming::Notification { method, params } if method == "session/cancel" => {
                let me = Arc::clone(self);
                pmt_core::spawn::spawn(async move { me.cancel(&params).await });
            }
            Incoming::Notification { method, .. } => {
                tracing::debug!(method, "ignored notification");
            }
            Incoming::Response { id, outcome } => {
                if self.out.settle(&id, outcome) {
                    return;
                }
                if self.options.unattended {
                    tracing::warn!(
                        "ignored a client answer on an unattended connection: it is never asked"
                    );
                } else {
                    tracing::debug!("ignored an answer to no request");
                }
            }
            Incoming::Invalid { id, code, message } => {
                self.out.reply(&id, Err(AcpError::new(code, message)));
            }
        }
    }

    async fn request(self: Arc<Self>, id: Value, method: &str, params: Value) {
        let outcome = match method {
            "initialize" => Ok(self.initialize(&params)),
            "authenticate" => self.authenticate(&params),
            "session/new" => return self.new_session(&id, &params).await,
            "session/load" | "session/resume" => return self.load(&id, &params).await,
            "session/fork" => return self.fork(&id, &params).await,
            "session/list" => self.list(&params).await,
            "session/prompt" => self.prompt(&params).await,
            "session/set_mode" => self.set_mode(&params).await,
            "session/set_model" => self.set_model(&params).await,
            "session/set_config_option" => self.set_config_option(&params).await,
            other => Err(Self::unknown(other)),
        };
        self.out.reply(&id, outcome);
    }

    fn unknown(method: &str) -> AcpError {
        if PROBES.contains(&method) {
            tracing::debug!(method, "liveness probe");
        } else {
            tracing::warn!(method, "unknown ACP method");
        }
        AcpError::new(METHOD_NOT_FOUND, format!("method not found: {method}"))
    }

    /// The client features this connection advertises to the daemon.
    fn features(&self) -> Vec<String> {
        let caps = self.caps();
        let mut features = Vec::new();
        if caps.read && caps.write {
            features.push(EDITOR_FS.to_owned());
        }
        if caps.terminal {
            features.push(EDITOR_TERMINAL.to_owned());
        }
        features
    }

    /// The daemon connection, made (and the held sessions re-attached)
    /// when there is none.
    ///
    /// # Errors
    ///
    /// The connection error; `profile_mismatch` when the daemon does not
    /// serve the launch profile (the connection is closed).
    pub(crate) async fn client(self: &Arc<Self>) -> Result<Client, AcpError> {
        let mut guard = self.conn.lock().await;
        if let Some(conn) = guard.as_ref().filter(|c| c.live()) {
            return Ok(conn.client.clone());
        }
        let before = guard.as_ref().map(|c| c.instance.clone());
        let features = self.features();
        let (client, inbox) = self
            .connector
            .connect(&features)
            .await
            .map_err(|e| self.connect_error(&e))?;
        let hello = client.hello();
        if !hello.profiles.iter().any(|p| *p == self.options.profile) {
            client.close();
            return Err(mismatch(&self.options.profile));
        }
        let instance = hello.instance_id.clone();
        let same = before.as_deref() == Some(instance.as_str());
        pmt_core::spawn::spawn(Arc::clone(self).pump(inbox));
        *guard = Some(Conn {
            client: client.clone(),
            instance,
        });
        drop(guard);
        if before.is_some() {
            self.reattach(&client, same).await;
        }
        Ok(client)
    }

    fn connect_error(&self, err: &pmt_core::error::Error) -> AcpError {
        let code = err.code().code;
        let unserved = code == "rpc.ticket" && err.message().contains("does not serve profile");
        if unserved || code == "profile.not_granted" {
            return mismatch(&self.options.profile);
        }
        AcpError::new(-32000, err.message())
    }

    /// The daemon instance of the current connection.
    pub(crate) async fn instance(&self) -> Option<String> {
        self.conn.lock().await.as_ref().map(|c| c.instance.clone())
    }

    async fn pump(self: Arc<Self>, mut inbox: Inbox) {
        let client = self.conn.lock().await.as_ref().map(|c| c.client.clone());
        loop {
            tokio::select! {
                event = inbox.events.recv() => match event {
                    Some(event) => self.on_event(&event),
                    None => break,
                },
                call = inbox.requests.recv() => match (call, &client) {
                    (Some(call), Some(client)) => {
                        let me = Arc::clone(&self);
                        let client = client.clone();
                        pmt_core::spawn::spawn(async move { me.on_server_request(client, call).await });
                    }
                    (Some(_), None) => {}
                    (None, _) => break,
                },
            }
        }
        self.on_lost();
    }

    /// The connection is gone: every admission in flight has an unknown
    /// outcome now.
    fn on_lost(&self) {
        let mut state = self.state();
        for (_, waiter) in state.waiters.drain() {
            let _ = waiter.send(Ended::Lost);
        }
        for session in state.sessions.values_mut() {
            if let Some(admission) = session.admission.take() {
                session.unknown = Some(admission);
            }
        }
    }

    fn on_event(self: &Arc<Self>, event: &Envelope) {
        let Some(daemon) = event.session_id.clone() else {
            return;
        };
        let mut state = self.state();
        let Some(editor) = state.by_daemon.get(&daemon).cloned() else {
            return;
        };
        let updates = match event.event.as_str() {
            "session.event" => session_event(&mut state, &editor, event),
            "turn.finished" => {
                drop(state);
                return self.turn_finished(&editor, &event.data);
            }
            "usage.updated" => {
                drop(state);
                let me = Arc::clone(self);
                let cost = event.data["root_totals"]["cost_usd"].as_f64();
                pmt_core::spawn::spawn(async move { me.usage(&editor, cost).await });
                return;
            }
            _ => Vec::new(),
        };
        let settings = settings_of(event);
        drop(state);
        for update in updates {
            self.out.update(&editor, update);
        }
        if let Some(settings) = settings {
            self.show_settings(&editor, settings);
        }
    }

    fn turn_finished(self: &Arc<Self>, editor: &str, data: &Value) {
        let Ok(finished) = serde_json::from_value::<TurnFinished>(data.clone()) else {
            return;
        };
        let mut state = self.state();
        let Some(session) = state.sessions.get_mut(editor) else {
            return;
        };
        let updates = session.projector.finish();
        if session
            .unknown
            .as_ref()
            .is_some_and(|u| u.turn_id.as_deref() == Some(finished.turn_id.as_str()))
        {
            // Its outcome is known now (replayed on re-attach).
            session.unknown = None;
        }
        let waiter = state.waiters.remove(&finished.turn_id);
        if waiter.is_none() {
            if state.early.len() == EARLY_KEPT {
                state.early.pop_front();
            }
            state.early.push_back(finished.clone());
        }
        drop(state);
        for update in updates {
            self.out.update(editor, update);
        }
        if let Some(waiter) = waiter {
            let _ = waiter.send(Ended::Finished(Box::new(finished)));
        }
        let me = Arc::clone(self);
        let editor = editor.to_owned();
        pmt_core::spawn::spawn(async move { me.refresh_settings(&editor).await });
    }

    /// Shows what changed in `settings` since the editor last saw them:
    /// the mode, the config options, the title.
    pub(crate) fn show_settings(&self, editor: &str, settings: SessionSettings) {
        let mut state = self.state();
        let Some(session) = state.sessions.get_mut(editor) else {
            return;
        };
        let updates = settings_updates(session, &settings);
        session.settings = Some(settings);
        drop(state);
        for update in updates {
            self.out.update(editor, update);
        }
    }

    /// Re-reads the session's settings and shows what changed.
    pub(crate) async fn refresh_settings(self: &Arc<Self>, editor: &str) {
        let Some(daemon) = self.daemon_id(editor) else {
            return;
        };
        let Ok(client) = self.client().await else {
            return;
        };
        let params = pmt_rpc::contracts::SessionRef { session_id: daemon };
        if let Ok(settings) = client
            .call_typed::<pmt_rpc::contracts::SessionSettingsCall>(&params)
            .await
        {
            self.show_settings(editor, settings);
        }
    }

    /// A usage update: the context the session fills and what the
    /// conversation cost (subagents included, from the usage ledger).
    pub(crate) async fn usage(self: &Arc<Self>, editor: &str, cost: Option<f64>) {
        let Some(daemon) = self.daemon_id(editor) else {
            return;
        };
        let Ok(client) = self.client().await else {
            return;
        };
        let params = pmt_rpc::contracts::SessionRef { session_id: daemon };
        let Ok(usage) = client
            .call_typed::<pmt_rpc::contracts::SessionContextUsage>(&params)
            .await
        else {
            return;
        };
        let mut update = json!({
            "sessionUpdate": "usage_update",
            "used": usage.tokens,
            "size": usage.context_window,
        });
        if let Some(amount) = cost.or(usage.cost_usd) {
            update["cost"] = json!({"amount": amount, "currency": "USD"});
        }
        self.out.update(editor, update);
    }

    /// The daemon session behind editor session `editor`.
    pub(crate) fn daemon_id(&self, editor: &str) -> Option<String> {
        self.state()
            .sessions
            .get(editor)
            .map(|s| s.daemon_id.clone())
    }

    /// The editor session of daemon session `daemon`.
    pub(crate) fn editor_of(&self, daemon: &str) -> Option<String> {
        self.state().by_daemon.get(daemon).cloned()
    }
}

/// A daemon that does not serve `profile`.
fn mismatch(profile: &str) -> AcpError {
    AcpError::new(
        -32002,
        format!("{PROFILE_MISMATCH}: the gateway daemon does not serve profile {profile}"),
    )
}

/// The projected updates of a `session.event` envelope.
fn session_event(state: &mut State, editor: &str, event: &Envelope) -> Vec<Value> {
    let Some(session) = state.sessions.get_mut(editor) else {
        return Vec::new();
    };
    session.seen = session.seen.max(event.seq);
    if event.data["type"] == SESSION_SETTINGS {
        return Vec::new();
    }
    session.projector.event(&event.data)
}

/// The settings a `session.settings` event carries.
fn settings_of(event: &Envelope) -> Option<SessionSettings> {
    (event.event == "session.event" && event.data["type"] == SESSION_SETTINGS)
        .then(|| serde_json::from_value(event.data["payload"].clone()).ok())
        .flatten()
}

/// The updates showing `new` to an editor that last saw `session`'s.
fn settings_updates(session: &mut Session, new: &SessionSettings) -> Vec<Value> {
    let mut updates = Vec::new();
    let old = session.settings.as_ref();
    let mode = crate::options::mode_of(new);
    if old.is_none_or(|old| crate::options::mode_of(old) != mode) {
        updates
            .push(json!({"sessionUpdate": "current_mode_update", "currentModeId": mode.as_str()}));
    }
    let shown = |s: &SessionSettings| {
        (
            s.provider.clone(),
            s.model.clone(),
            s.reasoning_effort.clone(),
            crate::options::mode_of(s),
        )
    };
    if old.is_none_or(|old| shown(old) != shown(new)) {
        updates.push(json!({
            "sessionUpdate": "config_option_update",
            "configOptions": crate::options::config_options(new),
        }));
    }
    if let Some(title) = new.title.as_ref().filter(|t| !t.is_empty())
        && session.title.as_ref() != Some(title)
    {
        session.title = Some(title.clone());
        updates.push(json!({"sessionUpdate": "session_info_update", "title": title}));
    }
    updates
}

#[cfg(test)]
mod tests {
    use pmt_rpc::contracts::{EditMode, SessionSettings};

    use super::{Session, settings_updates};

    fn settings(title: Option<&str>, mode: EditMode) -> SessionSettings {
        SessionSettings {
            session_id: "ses_x".into(),
            provider: "mock".into(),
            model: "mock".into(),
            models: Vec::new(),
            reasoning_effort: "medium".into(),
            reasoning_efforts: Vec::new(),
            edit_mode: Some(mode),
            cwd: "/w".into(),
            busy: false,
            title: title.map(str::to_owned),
        }
    }

    /// Scenario: Same title republished.
    #[test]
    fn an_unchanged_title_is_not_sent_again() {
        let mut session = Session::default();
        let first = settings(Some("Fix the build"), EditMode::Default);
        let shown = settings_updates(&mut session, &first);
        assert!(
            shown
                .iter()
                .any(|u| u["sessionUpdate"] == "session_info_update")
        );
        session.settings = Some(first.clone());
        assert!(settings_updates(&mut session, &first).is_empty());
    }

    /// Scenario: Mode changed in the desktop.
    #[test]
    fn a_mode_change_from_another_surface_is_a_current_mode_update() {
        let mut session = Session {
            settings: Some(settings(None, EditMode::Default)),
            ..Session::default()
        };
        let updates = settings_updates(&mut session, &settings(None, EditMode::AcceptEdits));
        assert_eq!(updates[0]["sessionUpdate"], "current_mode_update");
        assert_eq!(updates[0]["currentModeId"], "accept_edits");
        assert!(
            updates
                .iter()
                .any(|u| u["sessionUpdate"] == "config_option_update")
        );
    }
}
