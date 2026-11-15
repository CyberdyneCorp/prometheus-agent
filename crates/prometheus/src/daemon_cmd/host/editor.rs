//! Editor sessions in the daemon (spec: acp-adapter): what an ACP
//! session keeps besides its messages, the settings an editor shows and
//! changes, and cancelling one editor's admission.
//!
//! An editor session's policy (its editor MCP servers and its edit mode)
//! is stored per session in the store's preferences, so it survives the
//! session's unload and a daemon restart and a second editor loading the
//! session finds the servers the first one bound. Settings changes made
//! by any surface are announced on the session as a `session.settings`
//! event, which the ACP adapter turns into mode and config updates.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_gateway::Link;
use pmt_gateway::editor::{EditorCall, EditorReply};
use pmt_rpc::contracts::{
    ConfigureParams, Configured, DispatchParams, Dispatched, EditMode, EditorBindParams,
    EditorBound, EditorMcpServer, SESSION_SETTINGS, SessionEvent, SessionSettings, TurnEnded,
    TurnFinished,
};
use pmt_store::Store;
use pmt_tools::approval::edits::EditPolicy;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{DaemonHost, Loaded, Profile, session_id};
use crate::chat::session::tools::ACP_UNATTENDED_TAG;

/// The preference key prefix of a session's editor policy.
const PREF: &str = "acp.editor.";

/// What the daemon refuses while a turn runs.
const MODE_BUSY: &str = "Mode changes apply when the session is idle; try again after this turn.";

/// A session's editor policy as stored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct EditorState {
    /// The editor MCP servers bound to the session.
    #[serde(default)]
    pub(super) mcp_servers: Vec<EditorMcpServer>,
    /// The edit mode (`None`: `default`).
    #[serde(default)]
    pub(super) edit_mode: Option<EditMode>,
}

/// The editor policy of session `id`; an unreadable one is empty.
pub(super) fn state(store: &Store, id: SessionId) -> EditorState {
    store
        .pref(&format!("{PREF}{id}"))
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save(store: &Store, id: SessionId, state: &EditorState) -> Result<(), Error> {
    let text = serde_json::to_string(state).unwrap_or_default();
    store.set_pref(&format!("{PREF}{id}"), &text)
}

/// Whether stored session `row` is an interactive editor session (an ACP
/// session a person drives): those ask before edits by default.
/// Unattended ACP sessions follow the profile's approval policy alone.
pub(super) fn is_editor(row: &pmt_store::Session) -> bool {
    row.source == pmt_store::Source::Acp && row.source_tag.as_deref() != Some(ACP_UNATTENDED_TAG)
}

/// The gate policy of edit mode `mode`.
const fn policy_of(mode: EditMode) -> EditPolicy {
    match mode {
        EditMode::Default => EditPolicy::Ask,
        EditMode::AcceptEdits => EditPolicy::AcceptInWorkspace,
        EditMode::DontAsk => EditPolicy::DontAsk,
    }
}

/// Records the editor servers a new session was created with.
pub(super) fn created(
    store: &Store,
    id: SessionId,
    servers: &[EditorMcpServer],
) -> Result<(), Error> {
    if servers.is_empty() {
        return Ok(());
    }
    let state = EditorState {
        mcp_servers: servers.to_vec(),
        edit_mode: None,
    };
    save(store, id, &state)
}

/// A fork keeps its parent's editor policy (its editor servers, its edit
/// mode) until an editor binds others.
pub(super) fn forked(store: &Store, parent: SessionId, child: SessionId) -> Result<(), Error> {
    let state = state(store, parent);
    if state == EditorState::default() {
        return Ok(());
    }
    save(store, child, &state)
}

/// Applies an editor session's stored policy to the session just loaded:
/// its edit mode to the approval gate, its editor servers to its tools.
pub(super) fn on_load(profile: &Profile, row: &pmt_store::Session, session: &Loaded, link: &Link) {
    if row.source != pmt_store::Source::Acp {
        return;
    }
    super::editor_bridge::bind(&profile.scope(), &row.id.to_string(), link);
    let state = state(&profile.store, row.id);
    if is_editor(row) {
        let mode = state.edit_mode.unwrap_or(EditMode::Default);
        session.live.gate.set_edit_policy(Some(policy_of(mode)));
    }
    crate::mcp_host::editor::bind(&profile.scope(), &row.id.to_string(), &state.mcp_servers);
}

/// The edit mode a loaded session's gate applies.
fn mode_of(session: &Loaded) -> Option<EditMode> {
    session.live.gate.edit_policy().map(|policy| match policy {
        EditPolicy::Ask => EditMode::Default,
        EditPolicy::AcceptInWorkspace => EditMode::AcceptEdits,
        EditPolicy::DontAsk => EditMode::DontAsk,
    })
}

/// Announces `settings` on the session to every attached client.
fn publish(link: &Link, settings: &SessionSettings) {
    link.emit::<SessionEvent>(&json!({
        "type": SESSION_SETTINGS,
        "ts": jiff::Timestamp::now().to_string(),
        "turn_id": null,
        "span_id": null,
        "parent_span_id": null,
        "payload": settings,
    }));
}

fn busy(message: &str) -> Error {
    Error::new(&codes::SESSION_BUSY, message.to_owned())
}

/// A refused `/model` or `/reasoning` as an error the client shows.
fn refusal(reason: String) -> Error {
    Error::new(&codes::RPC_INVALID_PARAMS, reason)
}

impl DaemonHost {
    pub(super) async fn editor_request(
        &self,
        profile: &Profile,
        request: EditorCall,
        link: Link,
    ) -> Result<EditorReply, Error> {
        let id = session_id(request.session_id())?;
        let session = self.load(profile, id, &link).await?;
        match request {
            EditorCall::Bind(params) => Ok(EditorReply::Bound(Self::bind(profile, id, &params)?)),
            EditorCall::Settings(_) => Ok(EditorReply::Settings(
                self.settings_of(profile, &session).await,
            )),
            EditorCall::Configure(params) => {
                Box::pin(self.configure(profile, &session, params, link))
                    .await
                    .map(EditorReply::Configured)
            }
        }
    }

    /// `session.editor`: a non-empty list replaces the session's editor
    /// servers (from its next turn), an empty one keeps them.
    fn bind(
        profile: &Profile,
        id: SessionId,
        params: &EditorBindParams,
    ) -> Result<EditorBound, Error> {
        let mut state = state(&profile.store, id);
        let changed = !params.mcp_servers.is_empty() && params.mcp_servers != state.mcp_servers;
        if changed {
            state.mcp_servers.clone_from(&params.mcp_servers);
            save(&profile.store, id, &state)?;
            crate::mcp_host::editor::bind(&profile.scope(), &id.to_string(), &state.mcp_servers);
        }
        Ok(EditorBound {
            servers: state.mcp_servers.iter().map(|s| s.name.clone()).collect(),
            changed,
        })
    }

    /// The settings of a loaded session.
    async fn settings_of(&self, profile: &Profile, session: &Loaded) -> SessionSettings {
        let target = session.live.model.target();
        let model = target.model.clone().unwrap_or_default();
        let id = session.live.id.to_string();
        let models = self
            .models_of(profile, Some(&id))
            .await
            .map(|options| options.models.into_iter().map(|m| m.id).collect())
            .unwrap_or_default();
        let config = profile.scope().config().agent.reasoning_effort;
        let effort = session.live.model.session().reasoning().or(config);
        SessionSettings {
            session_id: id,
            provider: target.provider.clone(),
            model,
            models,
            reasoning_effort: effort.map_or("none", |level| level.as_str()).to_owned(),
            reasoning_efforts: pmt_config::schema::ReasoningLevel::ALL
                .iter()
                .map(|level| level.as_str().to_owned())
                .collect(),
            edit_mode: mode_of(session),
            cwd: session.live.cwd().to_string_lossy().into_owned(),
            busy: session.busy.load(Ordering::Acquire),
            title: profile
                .store
                .session(session.live.id)
                .ok()
                .and_then(|row| row.title),
        }
    }

    /// `session.configure`: mode and cwd only while idle; a model switch
    /// during a turn applies once the turn ended.
    async fn configure(
        &self,
        profile: &Profile,
        session: &Arc<Loaded>,
        params: ConfigureParams,
        link: Link,
    ) -> Result<Configured, Error> {
        let running = session.busy.load(Ordering::Acquire);
        if let Some(mode) = params.edit_mode {
            if running {
                return Err(busy(MODE_BUSY));
            }
            session.live.gate.set_edit_policy(Some(policy_of(mode)));
            let mut state = state(&profile.store, session.live.id);
            state.edit_mode = Some(mode);
            save(&profile.store, session.live.id, &state)?;
        }
        if let Some(cwd) = &params.cwd {
            if running {
                return Err(busy(
                    "the session is running a turn; its working directory stays",
                ));
            }
            let cwd = PathBuf::from(cwd);
            std::fs::create_dir_all(&cwd).map_err(|e| Error::io(&codes::HOME_IO, &cwd, &e))?;
            session.live.retarget(&session.factory, &cwd).await?;
        }
        if let Some(effort) = &params.reasoning_effort {
            self.run_command(profile, session, "reasoning", effort, &link)
                .await?;
        }
        let mut next_turn = false;
        if let Some(model) = params.model.clone() {
            if running {
                next_turn = true;
                self.switch_after_turn(profile, session, model, link.clone());
            } else {
                self.run_command(profile, session, "model", &model, &link)
                    .await?;
            }
        }
        let settings = self.settings_of(profile, session).await;
        publish(&link, &settings);
        Ok(Configured {
            settings,
            next_turn,
        })
    }

    /// Runs a session command for `session.configure`; a refusal is the
    /// error the client shows.
    async fn run_command(
        &self,
        profile: &Profile,
        session: &Loaded,
        name: &str,
        args: &str,
        link: &Link,
    ) -> Result<(), Error> {
        let params = DispatchParams {
            session_id: session.live.id.to_string(),
            name: name.to_owned(),
            args: args.to_owned(),
        };
        match self.dispatch_command(profile, params, link.clone()).await? {
            Dispatched::Refused { reason } => Err(refusal(reason)),
            Dispatched::Done { .. } | Dispatched::Send { .. } => Ok(()),
        }
    }

    /// A model switch asked for during a turn: applied once the turn
    /// ended (the running turn keeps its model), then announced.
    fn switch_after_turn(
        &self,
        profile: &Profile,
        session: &Arc<Loaded>,
        model: String,
        link: Link,
    ) {
        let (Some(host), name) = (self.arc(), profile.scope().name().to_string()) else {
            return;
        };
        let session = Arc::clone(session);
        pmt_core::spawn::spawn(async move {
            session.until_idle().await;
            let Ok(profile) = host.profile(&name) else {
                return;
            };
            match host
                .run_command(&profile, &session, "model", &model, &link)
                .await
            {
                Ok(()) => {
                    let settings = host.settings_of(&profile, &session).await;
                    publish(&link, &settings);
                }
                Err(err) => super::commands::notice(&link, err.message()),
            }
        });
    }

    /// Announces the session's settings after a command another surface
    /// ran may have changed them (`/model`, `/provider`, `/reasoning`).
    pub(super) async fn settings_changed(&self, profile: &Profile, session: &Loaded, link: &Link) {
        let settings = self.settings_of(profile, session).await;
        publish(link, &settings);
    }

    /// Cancels admission `turn_id` of session `id`: the running turn stops
    /// only when it is that admission; a queued one leaves the queue and
    /// ends as interrupted. Any other turn keeps running.
    pub(super) async fn cancel_admission(
        &self,
        profile: &Profile,
        id: SessionId,
        turn_id: &str,
    ) -> Result<(), Error> {
        let Some(session) = profile.loaded.lock().await.get(&id).cloned() else {
            return Ok(());
        };
        let running = session.current.lock().expect("current poisoned").clone();
        if running.as_deref() == Some(turn_id) {
            session.stop();
            return Ok(());
        }
        let removed = {
            let mut queue = session.queue.lock().expect("queue poisoned");
            let at = queue.iter().position(|job| job.turn_id == turn_id);
            at.and_then(|at| queue.remove(at))
        };
        if let Some(job) = removed {
            session.queue_changed(&job.link);
            job.link.emit::<TurnEnded>(&TurnFinished {
                turn_id: turn_id.to_owned(),
                exit_reason: "interrupted_by_user".to_owned(),
                final_response: None,
                error: None,
                usage: None,
                report: None,
            });
        }
        Ok(())
    }
}
