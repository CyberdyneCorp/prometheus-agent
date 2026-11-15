//! ACP session management over daemon sessions (spec: acp-adapter,
//! Initialize handshake; Authentication reuses profile credentials; New
//! session on the daemon; Load and resume replay the logical session;
//! Editor MCP servers belong to the daemon session; Session list is a
//! paged read-only catalog; Fork creates a daemon branch; Edit-approval
//! modes; Config options are validated; Model switching through the
//! daemon).

use std::sync::Arc;

use pmt_rpc::client::Client;
use pmt_rpc::contracts::{
    ClientKind, CommandsCatalog, ConfigureParams, EditMode, EditorBindParams, EditorMcpServer,
    ForkParams, NameValue, ResolveKind, ResolveParams, ResumeParams, SessionConfigure,
    SessionCreate, SessionCreateParams, SessionEditor, SessionFork, SessionListParams,
    SessionListing, SessionRef, SessionResolve, SessionResume, SessionSettings,
    SessionSettingsCall, SessionSetup, SessionTranscript,
};
use serde_json::{Value, json};

use crate::adapter::{Adapter, ClientCaps};
use crate::frames::AcpError;

/// The ACP protocol version this adapter speaks.
pub(crate) const PROTOCOL_VERSION: u64 = 1;

/// The source tag of unattended ACP sessions.
pub(crate) const UNATTENDED_TAG: &str = "acp-unattended";

/// What `--safe-mode` without a model answers `session/new`.
const SAFE_MODE: &str =
    "--safe-mode reads no profile default model: launch with --model <m> --provider <p>";

/// What a busy session tells an editor whose cwd differs.
const BUSY_CWD: &str = "Attached to the running session; editor cwd is not applied.";

/// What a changed editor server list tells the editor.
const MCP_CHANGED: &str = "Editor MCP tools changed; the new tool list applies from the next turn.";

fn text(params: &Value, key: &str) -> Result<String, AcpError> {
    params[key]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| AcpError::invalid(format!("missing {key}")))
}

fn rpc(err: &pmt_rpc::RpcError) -> AcpError {
    AcpError::of(err)
}

/// The editor's `mcpServers` as the daemon takes them.
fn servers(params: &Value) -> Vec<EditorMcpServer> {
    let pairs = |value: &Value| -> Vec<NameValue> {
        value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| {
                Some(NameValue {
                    name: v["name"].as_str()?.to_owned(),
                    value: v["value"].as_str()?.to_owned(),
                })
            })
            .collect()
    };
    params["mcpServers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let transport = s["type"].as_str().unwrap_or(if s.get("url").is_some() {
                "http"
            } else {
                "stdio"
            });
            Some(EditorMcpServer {
                name: s["name"].as_str()?.to_owned(),
                transport: transport.to_owned(),
                command: s["command"].as_str().map(str::to_owned),
                args: s["args"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|a| a.as_str().map(str::to_owned))
                    .collect(),
                env: pairs(&s["env"]),
                url: s["url"].as_str().map(str::to_owned),
                headers: pairs(&s["headers"]),
            })
        })
        .collect()
}

/// The answer of `session/new`, `session/load` and `session/fork`.
fn session_answer(settings: &SessionSettings, session_id: Option<&str>) -> Value {
    let mut answer = json!({
        "models": crate::options::models(settings),
        "modes": crate::options::modes(settings),
        "configOptions": crate::options::config_options(settings),
    });
    if let Some(id) = session_id {
        answer["sessionId"] = json!(id);
    }
    answer
}

impl Adapter {
    pub(crate) fn initialize(&self, params: &Value) -> Value {
        let client = &params["clientCapabilities"];
        self.set_caps(ClientCaps {
            read: client["fs"]["readTextFile"].as_bool() == Some(true),
            write: client["fs"]["writeTextFile"].as_bool() == Some(true),
            terminal: client["terminal"].as_bool() == Some(true),
            elicitation: client.get("elicitation").is_some_and(|e| !e.is_null()),
        });
        let methods: Vec<Value> = self
            .options
            .auth
            .methods()
            .into_iter()
            .map(|m| {
                let mut method = json!({"id": m.id, "name": m.name, "description": m.description});
                if let Some((command, args)) = m.terminal {
                    method["type"] = json!("terminal");
                    method["args"] = json!(args);
                    method["_meta"] = json!({"terminal-auth": {
                        "command": command, "args": args, "label": m.name,
                    }});
                }
                method
            })
            .collect();
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "agentInfo": {"name": "prometheus", "version": self.options.version},
            "agentCapabilities": {
                "loadSession": true,
                "promptCapabilities": {"image": true, "audio": false, "embeddedContext": true},
                "mcpCapabilities": {"http": true, "sse": false},
                "sessionCapabilities": {"list": {}, "fork": {}, "resume": {}},
            },
            "authMethods": methods,
        })
    }

    pub(crate) fn authenticate(&self, params: &Value) -> Result<Value, AcpError> {
        let method = text(params, "methodId")?;
        if self.options.auth.resolves(&method) {
            Ok(json!({}))
        } else {
            Err(AcpError::new(
                -32000,
                format!(
                    "authentication with {method} is not available: no credential resolves; run 'prometheus setup'"
                ),
            ))
        }
    }

    /// How sessions this process creates or loads run.
    fn setup(&self) -> Result<SessionSetup, AcpError> {
        let launch = &self.options.launch;
        if launch.safe_mode && (launch.model.is_none() || launch.provider.is_none()) {
            return Err(AcpError::new(-32000, SAFE_MODE));
        }
        let mut set = Vec::new();
        if launch.safe_mode {
            set.push("plugins.safe_mode=true".to_owned());
        }
        Ok(SessionSetup {
            model: launch.model.clone(),
            provider: launch.provider.clone(),
            set,
            yolo: launch.yolo,
            ignore_user_config: launch.ignore_user_config,
            ..SessionSetup::default()
        })
    }

    pub(crate) async fn new_session(self: &Arc<Self>, id: &Value, params: &Value) {
        match self.create(params).await {
            Ok((editor, answer)) => {
                self.out.reply(id, Ok(answer));
                self.after_attach(&editor).await;
            }
            Err(err) => self.out.reply(id, Err(err)),
        }
    }

    async fn create(self: &Arc<Self>, params: &Value) -> Result<(String, Value), AcpError> {
        let setup = self.setup()?;
        let cwd = text(params, "cwd")?;
        let client = self.client().await?;
        let create = SessionCreateParams {
            cwd: Some(cwd),
            source: Some(ClientKind::Acp),
            source_tag: self.options.unattended.then(|| UNATTENDED_TAG.to_owned()),
            idempotency_key: Some(ulid::Ulid::generate().to_string()),
            setup: setup.clone(),
            editor_mcp_servers: servers(params),
            ..SessionCreateParams::default()
        };
        let created = client
            .call_typed::<SessionCreate>(&create)
            .await
            .map_err(|e| rpc(&e))?;
        let daemon = created.session_id;
        self.attach(&client, &daemon, &daemon, Some(setup)).await?;
        let settings = self.settings(&client, &daemon).await?;
        Ok((daemon.clone(), session_answer(&settings, Some(&daemon))))
    }

    /// Loads daemon session `daemon` (attached as a driver, never its
    /// exclusive owner) for editor session `editor` and reads its
    /// commands.
    async fn attach(
        &self,
        client: &Client,
        editor: &str,
        daemon: &str,
        setup: Option<SessionSetup>,
    ) -> Result<(), AcpError> {
        let resume = ResumeParams {
            session_id: daemon.to_owned(),
            exclusive: false,
            setup,
        };
        let attached = client
            .call_typed::<SessionResume>(&resume)
            .await
            .map_err(|e| rpc(&e))?;
        let reference = SessionRef {
            session_id: daemon.to_owned(),
        };
        let commands = client
            .call_typed::<CommandsCatalog>(&reference)
            .await
            .map(|c| c.commands)
            .unwrap_or_default();
        let mut state = self.state();
        let old = state.sessions.remove(editor);
        if let Some(old) = &old {
            state.by_daemon.remove(&old.daemon_id);
        }
        let mut session = old.unwrap_or_default();
        daemon.clone_into(&mut session.daemon_id);
        session.commands = commands;
        session.seen = attached.latest_seq;
        state.by_daemon.insert(daemon.to_owned(), editor.to_owned());
        state.sessions.insert(editor.to_owned(), session);
        Ok(())
    }

    async fn settings(&self, client: &Client, daemon: &str) -> Result<SessionSettings, AcpError> {
        let reference = SessionRef {
            session_id: daemon.to_owned(),
        };
        client
            .call_typed::<SessionSettingsCall>(&reference)
            .await
            .map_err(|e| rpc(&e))
    }

    /// After an attach: the advertised commands (re-sent after every
    /// attach), the settings and a usage update.
    pub(crate) async fn after_attach(self: &Arc<Self>, editor: &str) {
        let commands = self
            .state()
            .sessions
            .get(editor)
            .map(|s| crate::slash::advertised(&s.commands));
        if let Some(update) = commands {
            self.out.update(editor, update);
        }
        self.refresh_settings(editor).await;
        self.usage(editor, None).await;
    }

    pub(crate) async fn load(self: &Arc<Self>, id: &Value, params: &Value) {
        match self.load_session(params).await {
            Ok((editor, answer, notes)) => {
                self.out.reply(id, Ok(answer));
                for note in notes {
                    self.say(&editor, note);
                }
                self.after_attach(&editor).await;
            }
            Err(err) => self.out.reply(id, Err(err)),
        }
    }

    /// `session/load` and `session/resume`: attaches the editor-held id's
    /// current lineage tip and replays its transcript before answering.
    async fn load_session(
        self: &Arc<Self>,
        params: &Value,
    ) -> Result<(String, Value, Vec<&'static str>), AcpError> {
        let setup = self.setup()?;
        let editor = text(params, "sessionId")?;
        let cwd = params["cwd"].as_str().map(str::to_owned);
        let client = self.client().await?;
        let resolve = ResolveParams {
            profile: None,
            kind: ResolveKind::Tip,
            reference: editor.clone(),
            cwd: None,
            terminal: None,
        };
        let tip = client
            .call_typed::<SessionResolve>(&resolve)
            .await
            .map_err(|e| rpc(&e))?
            .session_id;
        // A session keeps how it was set up unless the launch flags say
        // otherwise (a busy session cannot be set up again).
        let setup = Some(setup).filter(|s| *s != SessionSetup::default());
        self.attach(&client, &editor, &tip, setup).await?;
        self.replay(&client, &editor, &tip).await;
        let mut notes = Vec::new();
        let mut settings = self.settings(&client, &tip).await?;
        if let Some(cwd) = cwd.filter(|c| *c != settings.cwd) {
            if settings.busy {
                notes.push(BUSY_CWD);
            } else {
                settings = self
                    .configure(
                        &client,
                        ConfigureParams {
                            session_id: tip.clone(),
                            cwd: Some(cwd),
                            ..ConfigureParams::default()
                        },
                    )
                    .await?;
            }
        }
        if self.bind_servers(&client, &tip, servers(params)).await? {
            notes.push(MCP_CHANGED);
        }
        Ok((editor, session_answer(&settings, None), notes))
    }

    /// Replays the session's stored conversation to the editor.
    async fn replay(&self, client: &Client, editor: &str, daemon: &str) {
        let reference = SessionRef {
            session_id: daemon.to_owned(),
        };
        let Ok(transcript) = client.call_typed::<SessionTranscript>(&reference).await else {
            return;
        };
        for (index, message) in transcript.messages.iter().enumerate() {
            let text = message.text.as_deref().unwrap_or_default();
            if let Some(update) = crate::project::replayed(&message.role, text, index) {
                self.out.update(editor, update);
            }
        }
    }

    /// Binds the editor's servers (a non-empty list replaces, an empty one
    /// keeps); `true` when the list changed.
    async fn bind_servers(
        &self,
        client: &Client,
        daemon: &str,
        servers: Vec<EditorMcpServer>,
    ) -> Result<bool, AcpError> {
        let params = EditorBindParams {
            session_id: daemon.to_owned(),
            mcp_servers: servers,
        };
        let bound = client
            .call_typed::<SessionEditor>(&params)
            .await
            .map_err(|e| rpc(&e))?;
        Ok(bound.changed)
    }

    async fn configure(
        &self,
        client: &Client,
        params: ConfigureParams,
    ) -> Result<SessionSettings, AcpError> {
        client
            .call_typed::<SessionConfigure>(&params)
            .await
            .map(|c| c.settings)
            .map_err(|e| rpc(&e))
    }

    pub(crate) async fn fork(self: &Arc<Self>, id: &Value, params: &Value) {
        match self.fork_session(params).await {
            Ok((child, answer)) => {
                self.out.reply(id, Ok(answer));
                self.after_attach(&child).await;
            }
            Err(err) => self.out.reply(id, Err(err)),
        }
    }

    /// `session/fork`: a daemon branch, moved to the editor cwd, attached,
    /// with the given editor servers or else the parent's.
    async fn fork_session(self: &Arc<Self>, params: &Value) -> Result<(String, Value), AcpError> {
        let setup = self.setup()?;
        let editor = text(params, "sessionId")?;
        let parent = self
            .daemon_id(&editor)
            .ok_or_else(|| not_attached(&editor))?;
        let client = self.client().await?;
        let fork = ForkParams {
            session_id: parent,
            at_message_id: None,
            title: None,
        };
        let child = client
            .call_typed::<SessionFork>(&fork)
            .await
            .map_err(|e| rpc(&e))?
            .session_id;
        let setup = Some(setup).filter(|s| *s != SessionSetup::default());
        self.attach(&client, &child, &child, setup).await?;
        let mut settings = self.settings(&client, &child).await?;
        if let Some(cwd) = params["cwd"].as_str().filter(|c| *c != settings.cwd) {
            settings = self
                .configure(
                    &client,
                    ConfigureParams {
                        session_id: child.clone(),
                        cwd: Some(cwd.to_owned()),
                        ..ConfigureParams::default()
                    },
                )
                .await?;
        }
        self.bind_servers(&client, &child, servers(params)).await?;
        Ok((child.clone(), session_answer(&settings, Some(&child))))
    }

    /// `session/list`: the launch profile's ACP sessions with messages,
    /// one per conversation, newest first, 50 a page.
    pub(crate) async fn list(self: &Arc<Self>, params: &Value) -> Result<Value, AcpError> {
        let client = self.client().await?;
        let list = SessionListParams {
            limit: Some(50),
            source: Some(ClientKind::Acp),
            cwd: params["cwd"].as_str().map(str::to_owned),
            min_messages: Some(1),
            tips_only: true,
            cursor: params["cursor"].as_str().map(str::to_owned),
            ..SessionListParams::default()
        };
        let page = client
            .call_typed::<SessionListing>(&list)
            .await
            .map_err(|e| rpc(&e))?;
        let sessions: Vec<Value> = page
            .sessions
            .iter()
            .map(|s| {
                json!({
                    "sessionId": s.session_id,
                    "cwd": s.cwd,
                    "title": s.title,
                    "updatedAt": s.updated_at,
                })
            })
            .collect();
        Ok(json!({"sessions": sessions, "nextCursor": page.next_cursor}))
    }

    pub(crate) async fn set_mode(self: &Arc<Self>, params: &Value) -> Result<Value, AcpError> {
        let editor = text(params, "sessionId")?;
        let mode = text(params, "modeId")?;
        let mode = EditMode::parse(&mode)
            .ok_or_else(|| AcpError::invalid(format!("unknown mode {mode}")))?;
        let daemon = self
            .daemon_id(&editor)
            .ok_or_else(|| not_attached(&editor))?;
        let client = self.client().await?;
        let settings = self
            .configure(
                &client,
                ConfigureParams {
                    session_id: daemon,
                    edit_mode: Some(mode),
                    ..ConfigureParams::default()
                },
            )
            .await?;
        self.show_settings(&editor, settings);
        Ok(json!({}))
    }

    pub(crate) async fn set_model(self: &Arc<Self>, params: &Value) -> Result<Value, AcpError> {
        let editor = text(params, "sessionId")?;
        let model = text(params, "modelId")?;
        if !model.contains('/') {
            return Err(AcpError::invalid(format!(
                "model {model} is not provider-qualified: use <provider>/<model>"
            )));
        }
        let daemon = self
            .daemon_id(&editor)
            .ok_or_else(|| not_attached(&editor))?;
        let client = self.client().await?;
        let params = ConfigureParams {
            session_id: daemon,
            model: Some(model),
            ..ConfigureParams::default()
        };
        match client.call_typed::<SessionConfigure>(&params).await {
            Ok(done) => {
                if done.next_turn {
                    self.say(&editor, "Model change applies from the next turn.");
                }
                self.show_settings(&editor, done.settings);
                Ok(json!({}))
            }
            Err(err) => {
                self.say(&editor, &format!("Model not changed: {}", err.message));
                Err(rpc(&err))
            }
        }
    }

    pub(crate) async fn set_config_option(
        self: &Arc<Self>,
        params: &Value,
    ) -> Result<Value, AcpError> {
        let editor = text(params, "sessionId")?;
        let id = text(params, "configId")?;
        let value = params["value"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| AcpError::invalid("missing value"))?;
        let daemon = self
            .daemon_id(&editor)
            .ok_or_else(|| not_attached(&editor))?;
        let client = self.client().await?;
        let current = self.settings(&client, &daemon).await?;
        let change = crate::options::change(&current, &id, &value)?;
        let settings = self.configure(&client, change).await?;
        let options = crate::options::config_options(&settings);
        self.show_settings(&editor, settings);
        Ok(json!({"configOptions": options}))
    }

    /// Shows `text` as an agent message of editor session `editor`.
    pub(crate) fn say(&self, editor: &str, text: &str) {
        self.out.update(
            editor,
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}),
        );
    }

    /// Re-attaches every held session after a new connection: from its
    /// watermark on the same daemon instance (the missed events are
    /// replayed), loaded again on a restarted one.
    pub(crate) async fn reattach(self: &Arc<Self>, client: &Client, same_instance: bool) {
        let held: Vec<(String, String, u64)> = self
            .state()
            .sessions
            .iter()
            .map(|(editor, s)| (editor.clone(), s.daemon_id.clone(), s.seen))
            .collect();
        for (editor, daemon, seen) in held {
            let attached = if same_instance {
                let params = pmt_rpc::contracts::AttachParams {
                    session_id: daemon.clone(),
                    role: pmt_rpc::contracts::AttachRole::Driver,
                    last_seen_seq: Some(seen),
                    epoch: None,
                };
                client
                    .call_typed::<pmt_rpc::contracts::SessionAttach>(&params)
                    .await
                    .map(|_| ())
                    .map_err(|e| rpc(&e))
            } else {
                self.attach(client, &editor, &daemon, None).await
            };
            if let Err(err) = attached {
                tracing::warn!(session = %daemon, "re-attach failed: {}", err.message);
                continue;
            }
            if !same_instance {
                self.settle_unknown(client, &editor, &daemon).await;
            }
            let commands = self
                .state()
                .sessions
                .get(&editor)
                .map(|s| crate::slash::advertised(&s.commands));
            if let Some(update) = commands {
                self.out.update(&editor, update);
            }
        }
    }

    /// After a daemon restart: an unknown admission whose prompt the
    /// stored conversation shows answered has a known outcome.
    async fn settle_unknown(&self, client: &Client, editor: &str, daemon: &str) {
        let Some(text) = self
            .state()
            .sessions
            .get(editor)
            .and_then(|s| s.unknown.as_ref().map(|u| u.text.clone()))
        else {
            return;
        };
        let reference = SessionRef {
            session_id: daemon.to_owned(),
        };
        let Ok(transcript) = client.call_typed::<SessionTranscript>(&reference).await else {
            return;
        };
        let last_user = transcript.messages.iter().rposition(|m| m.role == "user");
        let answered = last_user.is_some_and(|at| {
            transcript.messages[at].text.as_deref() == Some(text.as_str())
                && transcript.messages[at + 1..]
                    .iter()
                    .any(|m| m.role == "assistant")
        });
        if answered && let Some(session) = self.state().sessions.get_mut(editor) {
            session.unknown = None;
        }
    }
}

/// A session this process has not attached.
pub(crate) fn not_attached(editor: &str) -> AcpError {
    AcpError::new(
        -32002,
        format!("not_found: session {editor} is not attached by this editor connection"),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::servers;

    #[test]
    fn editor_servers_keep_their_command_env_and_transport() {
        let params = json!({"mcpServers": [
            {"name": "db", "command": "db-mcp", "args": ["--ro"], "env": [{"name": "DB", "value": "x"}]},
            {"type": "http", "name": "docs", "url": "https://docs.example/mcp", "headers": []}
        ]});
        let servers = servers(&params);
        assert_eq!(servers[0].transport, "stdio");
        assert_eq!(servers[0].env[0].value, "x");
        assert_eq!(servers[1].transport, "http");
        assert_eq!(servers[1].url.as_deref(), Some("https://docs.example/mcp"));
    }
}
