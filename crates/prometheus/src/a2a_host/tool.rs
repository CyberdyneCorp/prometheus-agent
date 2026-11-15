//! The `a2a_*` tools (spec: agent-interop, Outbound A2A tools): agents are
//! the profile's `[a2a.agents.<name>]` or explicit URLs, every call needs
//! a `net.connect` grant for the agent's host and goes through the egress
//! policy, exchanges are persisted in `state.db`, and replies come back as
//! untrusted tool output.

use std::sync::Arc;
use std::time::{Duration, Instant};

use pmt_a2a::client::Client;
use pmt_agent::tools::A2aPort;
use pmt_agent::turn::model::BoxFuture;
use pmt_config::ProfileScope;
use pmt_config::schema::{A2aAgent, MAX_ORCHESTRATE_AGENTS};
use pmt_core::capability::{Capability, CapabilitySet};
use pmt_core::provenance::Provenance;
use pmt_store::Store;
use pmt_store::a2a::A2aCall;
use pmt_tools::ToolResult;
use serde_json::{Value, json};

use super::transport::NetTransport;
use crate::kanban_host::goal::GoalJudge;

/// Timeout of a call to an agent given by URL.
const URL_TIMEOUT: Duration = Duration::from_mins(2);

/// One session's A2A port.
pub struct Port {
    scope: ProfileScope,
    store: Arc<Store>,
    capabilities: CapabilitySet,
    judge: Option<Arc<dyn GoalJudge>>,
}

/// The port of a session with `capabilities`, ranking `best` replies with
/// `judge`.
#[must_use]
pub fn port(
    scope: &ProfileScope,
    store: &Arc<Store>,
    capabilities: &CapabilitySet,
    judge: Option<Arc<dyn GoalJudge>>,
) -> Arc<dyn A2aPort> {
    Arc::new(Port {
        scope: scope.clone(),
        store: Arc::clone(store),
        capabilities: capabilities.clone(),
        judge,
    })
}

/// Where a call goes.
struct Target {
    /// The configured name, or the URL.
    label: String,
    url: String,
    token: Option<String>,
    timeout: Duration,
}

impl A2aPort for Port {
    fn call(&self, name: &str, arguments: Value) -> BoxFuture<'_, ToolResult> {
        let name = name.to_owned();
        Box::pin(async move {
            let text = |key: &str| arguments[key].as_str().unwrap_or_default().to_owned();
            match name.as_str() {
                "a2a_discover" => self.discover(&text("url")).await,
                "a2a_call" => {
                    let context = arguments["context_id"].as_str();
                    self.call_agent(&text("agent"), &text("message"), context)
                        .await
                }
                "a2a_list" => self.list(),
                "a2a_history" => self.history(&text("context_id")),
                "a2a_orchestrate" => {
                    self.orchestrate(&text("capability"), &text("message"), &text("mode"))
                        .await
                }
                other => ToolResult::error(format!("unknown A2A tool {other}")),
            }
        })
    }
}

fn untrusted(text: String, source: &str) -> ToolResult {
    let mut result = ToolResult::ok(text);
    result.metadata.insert(
        "provenance".to_owned(),
        Provenance::untrusted(format!("a2a:{source}")).to_value(),
    );
    result
}

impl Port {
    fn agents(&self) -> std::collections::BTreeMap<String, A2aAgent> {
        self.scope.config().a2a.agents.clone()
    }

    /// `agent` as a configured name or an http(s) URL.
    fn target(&self, agent: &str) -> Result<Target, String> {
        let agents = self.agents();
        if let Some(config) = agents.get(agent) {
            let token = match &config.auth_secret {
                Some(name) => Some(
                    self.scope
                        .secrets()
                        .resolve(&name.0)
                        .map_err(|e| format!("agent {agent}: {}", e.message()))?
                        .expose()
                        .to_owned(),
                ),
                None => None,
            };
            return Ok(Target {
                label: agent.to_owned(),
                url: config.url.clone(),
                token,
                timeout: Duration::from_secs(config.timeout_secs),
            });
        }
        if agent.starts_with("https://") || agent.starts_with("http://") {
            return Ok(Target {
                label: agent.to_owned(),
                url: agent.to_owned(),
                token: None,
                timeout: URL_TIMEOUT,
            });
        }
        let names: Vec<&str> = agents.keys().map(String::as_str).collect();
        Err(if names.is_empty() {
            format!(
                "unknown agent '{agent}': no agents are configured ([a2a.agents.<name>]); pass an http(s) URL instead"
            )
        } else {
            format!(
                "unknown agent '{agent}'; configured agents: {}",
                names.join(", ")
            )
        })
    }

    /// Refuses a URL whose host the session holds no `net.connect` grant
    /// for.
    fn granted(&self, url: &str) -> Result<(), String> {
        let parsed = url::Url::parse(url).map_err(|_| format!("'{url}' is not an http(s) URL"))?;
        let host = parsed.host_str().unwrap_or_default();
        let port = parsed.port_or_known_default().unwrap_or(443);
        let wanted = Capability::parse(&format!("net.connect:{host}:{port}"))
            .map_err(|e| e.message().to_owned())?;
        if self.capabilities.allows(&wanted) {
            Ok(())
        } else {
            Err(format!(
                "capability not granted: net.connect:{host} (grant it with `prometheus grants add net.connect:{host}`)"
            ))
        }
    }

    fn transport(&self, timeout: Duration) -> Result<NetTransport, String> {
        NetTransport::new(&self.scope, "a2a", timeout).map_err(|e| e.message().to_owned())
    }

    async fn discover(&self, url: &str) -> ToolResult {
        if let Err(why) = self.granted(url) {
            return ToolResult::error(why);
        }
        let transport = match self.transport(URL_TIMEOUT) {
            Ok(t) => t,
            Err(why) => return ToolResult::error(why),
        };
        match Client::new(&transport, None).discover(url).await {
            Ok((_, card)) => {
                let skills: Vec<Value> = card
                    .skills
                    .iter()
                    .map(|s| json!({"id": s.id, "name": s.name, "description": s.description}))
                    .collect();
                let summary = json!({
                    "name": card.name,
                    "description": card.description,
                    "url": card.url,
                    "protocol_version": card.protocol_version,
                    "streaming": card.streaming,
                    "push_notifications": card.push,
                    "authentication": card.auth,
                    "skills": skills,
                });
                untrusted(summary.to_string(), url)
            }
            Err(why) => ToolResult::error(why),
        }
    }

    /// One exchange with `target`, recorded.
    async fn exchange(
        &self,
        target: &Target,
        message: &str,
        context: Option<&str>,
    ) -> Result<(String, String), String> {
        self.granted(&target.url)?;
        let transport = self.transport(target.timeout)?;
        let started = Instant::now();
        let client = Client::new(&transport, target.token.as_deref());
        let called =
            tokio::time::timeout(target.timeout, client.call(&target.url, message, context))
                .await
                .unwrap_or_else(|_| Err(format!("no reply within {} s", target.timeout.as_secs())));
        let context_id = called
            .as_ref()
            .map(|c| c.context_id.clone())
            .ok()
            .or_else(|| context.map(str::to_owned))
            .unwrap_or_else(|| ulid::Ulid::generate().to_string().to_lowercase());
        let record = A2aCall {
            context_id: context_id.clone(),
            agent: target.label.clone(),
            url: target.url.clone(),
            task_id: called.as_ref().ok().and_then(|c| c.task_id.clone()),
            message: message.to_owned(),
            reply: called.as_ref().ok().map(|c| c.reply.clone()),
            error: called.as_ref().err().cloned(),
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            at: jiff::Timestamp::now(),
        };
        if let Err(err) = self.store.record_a2a_call(&record) {
            tracing::warn!("a2a exchange not recorded: {err}");
        }
        called.map(|c| (context_id, c.reply))
    }

    async fn call_agent(&self, agent: &str, message: &str, context: Option<&str>) -> ToolResult {
        let target = match self.target(agent) {
            Ok(target) => target,
            Err(why) => return ToolResult::error(why),
        };
        match self.exchange(&target, message, context).await {
            Ok((context_id, reply)) => untrusted(
                json!({"agent": target.label, "context_id": context_id, "reply": reply})
                    .to_string(),
                &target.label,
            ),
            Err(why) => ToolResult::error(format!("{}: {why}", target.label)),
        }
    }

    fn list(&self) -> ToolResult {
        let agents: Vec<Value> = self
            .agents()
            .iter()
            .map(|(name, a)| json!({"name": name, "url": a.url, "capabilities": a.capabilities, "timeout_secs": a.timeout_secs}))
            .collect();
        let conversations = self.store.a2a_conversations(50).unwrap_or_default();
        let conversations: Vec<Value> = conversations
            .iter()
            .map(|c| json!({"context_id": c.context_id, "agent": c.agent, "exchanges": c.exchanges, "last_at": c.last_at.to_string()}))
            .collect();
        let metrics: Vec<Value> = self
            .store
            .a2a_metrics()
            .unwrap_or_default()
            .iter()
            .map(|m| json!({"agent": m.agent, "calls": m.calls, "failures": m.failures, "mean_ms": m.mean_ms}))
            .collect();
        ToolResult::ok(
            json!({"agents": agents, "conversations": conversations, "metrics": metrics})
                .to_string(),
        )
    }

    fn history(&self, context_id: &str) -> ToolResult {
        match self.store.a2a_history(context_id) {
            Ok(calls) if calls.is_empty() => {
                ToolResult::error(format!("no A2A conversation has context_id '{context_id}'"))
            }
            Ok(calls) => {
                let agent = calls[0].agent.clone();
                let rows: Vec<Value> = calls
                    .iter()
                    .map(|c| json!({"at": c.at.to_string(), "agent": c.agent, "message": c.message, "reply": c.reply, "error": c.error}))
                    .collect();
                untrusted(
                    json!({"context_id": context_id, "exchanges": rows}).to_string(),
                    &agent,
                )
            }
            Err(err) => ToolResult::error(err.message().to_owned()),
        }
    }

    async fn orchestrate(&self, capability: &str, message: &str, mode: &str) -> ToolResult {
        if !matches!(mode, "all" | "first" | "best") {
            return ToolResult::error(format!("mode must be all, first or best (got '{mode}')"));
        }
        let names: Vec<String> = self
            .agents()
            .iter()
            .filter(|(_, a)| a.capabilities.iter().any(|c| c == capability))
            .map(|(name, _)| name.clone())
            .take(MAX_ORCHESTRATE_AGENTS)
            .collect();
        if names.is_empty() {
            return ToolResult::error(format!(
                "no configured agent advertises capability '{capability}'"
            ));
        }
        let replies = super::orchestrate::fan_out(self, &names, message, mode == "first").await;
        let replies = match mode {
            "best" => super::orchestrate::rank(self.judge.as_deref(), message, replies).await,
            _ => replies,
        };
        untrusted(replies.to_string(), &format!("orchestrate:{capability}"))
    }

    /// One call to configured agent `name` (orchestration).
    pub(super) async fn ask(&self, name: &str, message: &str) -> Result<String, String> {
        let target = self.target(name)?;
        self.exchange(&target, message, None)
            .await
            .map(|(_, reply)| reply)
    }
}
