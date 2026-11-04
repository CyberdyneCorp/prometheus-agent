//! The `manage_connections` tool (spec: connectors, `manage_connections`
//! tool; Connection flows require the user; Reconnect and rename
//! semantics).
//!
//! The tool checks and sets up connections; every change needs the user:
//! hosted accounts are authorized by the user through a card (surfaces
//! with one) or a link the model hands over (gateway chats, one-shot runs,
//! cron), and catalog MCP actions go through the approval gate (the
//! tool's guard escalates them, so a cron run resolves them as denied).
//! It has no action that removes an account. The turn loop runs it
//! ([`crate::Executor::AgentLoop`]) because the card is the session's.

mod args;
mod card;
mod hosted;

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use self::args::{Action, Request};
pub use self::card::{Card, CardAnswer, CardTarget};
pub use self::hosted::account_row;
use super::client::Gateway;
use crate::descriptor::{CallGuard, Effect, RegisteredTool, ResourceClaims, ToolDescriptor};
use crate::dispatch::Escalation;
use crate::result::{Status, ToolResult};
use crate::search::RemoteFuture;

/// The tool's name.
pub const NAME: &str = "manage_connections";
/// Its toolset.
pub const TOOLSET: &str = "connections";

/// How often a card's wait checks the gateway.
const POLL: Duration = Duration::from_secs(2);

/// The catalog MCP side of the tool, provided by the host (it writes the
/// profile's config and runs sign-ins).
pub trait McpTargets: Send + Sync {
    /// Installs catalog entry `name` (disabled until enabled).
    fn install<'a>(&'a self, name: &'a str) -> RemoteFuture<'a, Result<String, String>>;
    /// Enables configured server `name`.
    fn enable<'a>(&'a self, name: &'a str) -> RemoteFuture<'a, Result<String, String>>;
    /// Signs in to configured server `name`.
    fn authorize<'a>(&'a self, name: &'a str) -> RemoteFuture<'a, Result<String, String>>;
    /// The state of `name` (catalog entry, configured, enabled).
    fn status(&self, name: &str) -> Value;
}

/// Why hosted connectors are not offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// `connectors.enabled = false`.
    Disabled,
    /// No Nous sign-in.
    SignedOut,
}

/// The session's `manage_connections`.
pub struct Manager {
    hosted: Result<Gateway, Unavailable>,
    mcp: Option<Arc<dyn McpTargets>>,
    cards: bool,
    timeout: Duration,
    poll: Duration,
}

impl std::fmt::Debug for Manager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Manager")
            .field("hosted", &self.hosted)
            .field("cards", &self.cards)
            .finish_non_exhaustive()
    }
}

/// One authorization the user has to finish.
#[derive(Debug, Clone)]
pub(crate) struct Pending {
    pub(crate) connector: String,
    pub(crate) alias: Option<String>,
    pub(crate) connection_id: Option<String>,
    pub(crate) url: String,
}

impl Manager {
    /// The tool over `hosted` (the gateway, or why there is none), waiting
    /// `timeout` for a card.
    #[must_use]
    pub fn new(hosted: Result<Gateway, Unavailable>, timeout: Duration) -> Self {
        Self {
            hosted,
            mcp: None,
            cards: false,
            timeout,
            poll: POLL,
        }
    }

    /// The session's surface shows connection cards (a CLI terminal, the
    /// TUI, the desktop); without, the model gets links.
    #[must_use]
    pub fn with_cards(mut self) -> Self {
        self.cards = true;
        self
    }

    /// Whether calls take a card.
    #[must_use]
    pub fn uses_cards(&self) -> bool {
        self.cards
    }

    /// Catalog MCP targets are handled by `mcp`.
    #[must_use]
    pub fn with_mcp(mut self, mcp: Arc<dyn McpTargets>) -> Self {
        self.mcp = Some(mcp);
        self
    }

    /// Checks the gateway every `poll` while a card waits (tests).
    #[must_use]
    pub fn with_poll(mut self, poll: Duration) -> Self {
        self.poll = poll;
        self
    }

    /// Runs one call; `card` is the session's card when
    /// [`Self::uses_cards`] (else links are returned).
    pub async fn call(&self, arguments: &Value, card: Option<&dyn Card>) -> ToolResult {
        let request = match args::parse(arguments) {
            Ok(request) => request,
            Err(message) => return ToolResult::error(message),
        };
        let card = card.filter(|_| self.cards);
        match self.run(&request, card).await {
            Ok(answer) => ToolResult::ok(answer.to_string()),
            Err(message) => ToolResult::error(message),
        }
    }

    async fn run(&self, request: &Request, card: Option<&dyn Card>) -> Result<Value, String> {
        if request.action.is_mcp() {
            return self.mcp_action(request).await;
        }
        let gateway = self.gateway()?;
        let targets = &request.targets;
        let hosted: Vec<_> = targets.iter().filter(|t| !t.mcp).cloned().collect();
        let (pending, done) = match request.action {
            Action::Status => return self.status(gateway, request).await,
            Action::Display if card.is_none() => return self.status(gateway, request).await,
            Action::Display => self.display(gateway, &hosted).await?,
            Action::Connect => hosted::connect(gateway, &hosted)
                .await
                .map_err(|e| e.to_string())?,
            Action::Reconnect => hosted::reconnect(gateway, &hosted, request.force)
                .await
                .map_err(|e| e.to_string())?,
            Action::Rename => return hosted::rename(gateway, &hosted[0]).await,
            Action::Install | Action::Enable | Action::Authorize => unreachable!("handled above"),
        };
        Ok(self.finish(gateway, card, pending, done).await)
    }

    fn gateway(&self) -> Result<&Gateway, String> {
        self.hosted.as_ref().map_err(|why| match why {
            Unavailable::Disabled => "hosted connectors are turned off (connectors.enabled = false)".to_owned(),
            Unavailable::SignedOut => "hosted connectors need a Nous sign-in, which only the user can start: ask them to run `prometheus auth login nous`".to_owned(),
        })
    }

    async fn status(&self, gateway: &Gateway, request: &Request) -> Result<Value, String> {
        let mut answer = hosted::status(gateway, &request.targets)
            .await
            .map_err(|e| e.to_string())?;
        let mcp: Vec<Value> = match &self.mcp {
            Some(mcp) => request
                .targets
                .iter()
                .filter(|t| t.mcp)
                .map(|t| mcp.status(&t.name))
                .collect(),
            None => Vec::new(),
        };
        if !mcp.is_empty() {
            answer["mcp"] = json!(mcp);
        }
        Ok(answer)
    }

    /// `display` on a card surface: the card for the targets that are not
    /// connected (every such connector when none is named).
    async fn display(
        &self,
        gateway: &Gateway,
        targets: &[args::Target],
    ) -> Result<(Vec<Pending>, Vec<Value>), String> {
        let items = gateway.list().await.map_err(|e| e.to_string())?;
        let open: Vec<args::Target> = items
            .iter()
            .filter(|i| !i.connected)
            .filter(|i| targets.is_empty() || targets.iter().any(|t| t.name == i.connector))
            .map(|i| args::Target {
                name: i.connector.clone(),
                mcp: false,
                alias: targets
                    .iter()
                    .find(|t| t.name == i.connector)
                    .and_then(|t| t.alias.clone()),
                to: None,
            })
            .collect();
        let (pending, mut done) = hosted::connect(gateway, &open)
            .await
            .map_err(|e| e.to_string())?;
        done.extend(
            items
                .iter()
                .filter(|i| i.connected && targets.iter().any(|t| t.name == i.connector))
                .map(|i| json!({"connector": i.connector, "status": "connected"})),
        );
        Ok((pending, done))
    }

    async fn mcp_action(&self, request: &Request) -> Result<Value, String> {
        let mcp = self
            .mcp
            .as_ref()
            .ok_or_else(|| "no MCP catalog is available in this session".to_owned())?;
        let mut results = Vec::new();
        for target in &request.targets {
            let outcome = match request.action {
                Action::Install => mcp.install(&target.name).await,
                Action::Enable => mcp.enable(&target.name).await,
                _ => mcp.authorize(&target.name).await,
            };
            results.push(match outcome {
                Ok(message) => json!({"name": target.name, "status": "ok", "message": message}),
                Err(message) => {
                    json!({"name": target.name, "status": "failed", "message": message})
                }
            });
        }
        Ok(json!({"results": results}))
    }
}

/// The tool's registry entry (run by the turn loop).
#[must_use]
pub fn descriptor() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        NAME,
        TOOLSET,
        "Check and set up the user's connections: hosted app connectors (Gmail, Notion, Linear... run on the Nous connector gateway; their tools are found with tool_search as connectors__<app>__<tool>) and MCP servers from the catalog. \
         Actions: status (default) lists connectors and their accounts; display shows them to the user; connect, reconnect (only broken accounts; force: true switches a connected one) and rename (to: the new name) work on hosted connectors given by slug; install, enable and authorize work on catalog MCP servers given as {\"name\": ..., \"mcp\": true} and need the user's approval. \
         Every connection is finished by the user: where a card can be shown the call waits for it and returns connected, skipped or not_connected; elsewhere it returns a connect_url to give the user (never open it yourself). \
         If the user skipped or did not finish a connection, do not ask again on your own. \
         This tool cannot disconnect, delete or revoke accounts: the user runs `prometheus connectors disconnect` or uses the Nous portal. When you name a new account, tell the user the name you chose.",
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "status (default), display, connect, reconnect, rename, install, enable or authorize"
                },
                "connectors": {
                    "type": "array",
                    "items": {
                        "anyOf": [
                            {"type": "string"},
                            {
                                "type": "object",
                                "properties": {
                                    "name": {"type": "string"},
                                    "mcp": {"type": "boolean", "description": "A catalog MCP server (install, enable, authorize)"},
                                    "alias": {"type": "string", "description": "The account's name (^[a-z0-9][a-z0-9-]{0,31}$); for rename, its current name or label"},
                                    "to": {"type": "string", "description": "rename: the new name"}
                                },
                                "required": ["name"]
                            }
                        ]
                    },
                    "description": "Targets: connector slugs, or objects; at most one may name an account"
                },
                "force": {"type": "boolean", "description": "reconnect: also replace a connected account"}
            }
        }),
    );
    d.effect = Effect::External;
    d.claims = Some(ResourceClaims::none());
    d
}

/// The tool's registry entry with its approval guard: catalog MCP actions
/// (install, enable, authorize) always ask a human.
#[must_use]
pub fn registered() -> RegisteredTool {
    RegisteredTool::agent_loop(descriptor()).with_guard(guard())
}

fn guard() -> CallGuard {
    Arc::new(|_session: &str, arguments: &Value| {
        let action = arguments.get("action").and_then(Value::as_str)?;
        if !matches!(action, "install" | "enable" | "authorize") {
            return None;
        }
        let names: Vec<&str> = arguments
            .get("connectors")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|i| i.get("name").and_then(Value::as_str).or_else(|| i.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        Some(Escalation {
            rule_key: format!("{NAME}:{action}"),
            reason: format!("{action} MCP server {}", names.join(", ")),
            once_only: true,
        })
    })
}

/// The result of a call no loop can run (a session without the tool's
/// state).
#[must_use]
pub fn not_set_up() -> ToolResult {
    ToolResult::text(
        Status::Error,
        "manage_connections is not available in this session",
    )
}
