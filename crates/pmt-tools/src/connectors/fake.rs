//! A stateful fake of the Nous connector gateway (feature
//! `connectors-fake`), for tests of this crate and of the binary.
//!
//! It serves every route the client uses: search (substring match of the
//! query words on tool names, descriptions and connector slugs, after an
//! optional delay), schemas, execute (`CONNECTION_REQUIRED` without an
//! active account, `TOOL_NOT_FOUND` for unknown slugs), connections
//! (a new pending account per start; a repair retires the old account and
//! creates its replacement under the same alias), the connector list, the
//! accounts and their rename and removal. A pending account becomes active
//! when [`Fake::complete`] is called, or on the next accounts read with
//! [`Fake::complete_on_read`].

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::wire::{Account, ToolSchema};

#[derive(Debug, Default)]
struct State {
    connectors: Vec<String>,
    tools: Vec<ToolSchema>,
    accounts: Vec<Account>,
    next_id: u32,
    search_delay: Option<Duration>,
    complete_on_read: bool,
    rename_supported: bool,
    executed: Vec<Value>,
}

/// The running fake.
pub struct Fake {
    server: MockServer,
    state: Arc<Mutex<State>>,
}

impl std::fmt::Debug for Fake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { server, state } = self;
        f.debug_struct("Fake")
            .field("url", &server.uri())
            .field("state", state)
            .finish()
    }
}

/// A tool schema of the fake's catalog.
#[must_use]
pub fn tool(connector: &str, slug: &str, description: &str) -> ToolSchema {
    ToolSchema {
        connector: connector.to_owned(),
        tool: slug.to_owned(),
        description: description.to_owned(),
        input_schema: json!({"type": "object", "properties": {"to": {"type": "string"}, "body": {"type": "string"}}}),
    }
}

impl Fake {
    /// A gateway serving `connectors` with `tools`, renaming supported.
    pub async fn start(connectors: &[&str], tools: Vec<ToolSchema>) -> Self {
        let state = Arc::new(Mutex::new(State {
            connectors: connectors.iter().map(|&c| c.to_owned()).collect(),
            tools,
            rename_supported: true,
            ..State::default()
        }));
        let server = MockServer::start().await;
        let routes: [(&str, &str, Route); 8] = [
            ("POST", "/v1/connectors/search", Route::Search),
            ("POST", "/v1/connectors/schemas", Route::Schemas),
            ("POST", "/v1/connectors/execute", Route::Execute),
            ("POST", "/v1/connectors/connections", Route::Connect),
            ("GET", "/v1/connectors", Route::List),
            ("GET", "/v1/connectors/accounts", Route::Accounts),
            ("PATCH", "", Route::Rename),
            ("DELETE", "", Route::Remove),
        ];
        for (verb, route_path, route) in routes {
            let responder = Responder {
                state: Arc::clone(&state),
                route,
            };
            let mock = if route_path.is_empty() {
                Mock::given(method(verb)).and(path_regex("^/v1/connectors/accounts/[^/]+$"))
            } else {
                Mock::given(method(verb)).and(path(route_path))
            };
            mock.respond_with(responder).mount(&server).await;
        }
        Self { server, state }
    }

    /// The gateway's origin.
    #[must_use]
    pub fn url(&self) -> String {
        self.server.uri()
    }

    fn with<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        f(&mut self.state.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Adds an account; returns its id.
    #[allow(clippy::must_use_candidate, reason = "tests often ignore the id")]
    pub fn add_account(
        &self,
        connector: &str,
        alias: Option<&str>,
        label: &str,
        status: &str,
    ) -> String {
        self.with(|s| s.add(connector, alias, label, status))
    }

    /// Delays every search answer by `delay`.
    pub fn delay_search(&self, delay: Duration) {
        self.with(|s| s.search_delay = Some(delay));
    }

    /// Pending accounts become active on the next accounts read.
    pub fn complete_on_read(&self) {
        self.with(|s| s.complete_on_read = true);
    }

    /// The portal has no rename route (405).
    pub fn without_rename(&self) {
        self.with(|s| s.rename_supported = false);
    }

    /// Activates every pending account.
    pub fn complete(&self) {
        self.with(State::complete);
    }

    /// Every account.
    #[must_use]
    pub fn accounts(&self) -> Vec<Account> {
        self.with(|s| s.accounts.clone())
    }

    /// The execute requests' calls, in order.
    #[must_use]
    pub fn executed(&self) -> Vec<Value> {
        self.with(|s| s.executed.clone())
    }

    /// How many requests reached the gateway.
    pub async fn request_count(&self) -> usize {
        self.server.received_requests().await.map_or(0, |r| r.len())
    }
}

impl State {
    fn add(&mut self, connector: &str, alias: Option<&str>, label: &str, status: &str) -> String {
        self.next_id += 1;
        let id = format!("ca_{}", self.next_id);
        self.accounts.push(Account {
            connection_id: id.clone(),
            connector: connector.to_owned(),
            status: status.to_owned(),
            label: label.to_owned(),
            alias: alias.map(str::to_owned),
            disabled: false,
        });
        id
    }

    fn complete(&mut self) {
        for account in &mut self.accounts {
            if account.status == "pending" {
                "active".clone_into(&mut account.status);
            }
        }
    }

    fn active(&self, connector: &str) -> Vec<&Account> {
        self.accounts
            .iter()
            .filter(|a| a.connector == connector && a.is_active())
            .collect()
    }
}

#[derive(Debug, Clone, Copy)]
enum Route {
    Search,
    Schemas,
    Execute,
    Connect,
    List,
    Accounts,
    Rename,
    Remove,
}

struct Responder {
    state: Arc<Mutex<State>>,
    route: Route,
}

impl Respond for Responder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let segment = request.url.path().rsplit('/').next().unwrap_or_default();
        let id = percent_encoding::percent_decode_str(segment)
            .decode_utf8_lossy()
            .into_owned();
        match self.route {
            Route::Search => search(&state, &body),
            Route::Schemas => schemas(&state, &body),
            Route::Execute => execute(&mut state, &body),
            Route::Connect => connect(&mut state, &body),
            Route::List => {
                let items: Vec<Value> = state
                    .connectors
                    .iter()
                    .map(|c| json!({"connector": c, "connected": !state.active(c).is_empty()}))
                    .collect();
                ok(&json!({"items": items}))
            }
            Route::Accounts => {
                if state.complete_on_read {
                    state.complete();
                }
                ok(&json!({"accounts": state.accounts}))
            }
            Route::Rename => rename(&mut state, &id, &body),
            Route::Remove => {
                let before = state.accounts.len();
                state.accounts.retain(|a| a.connection_id != id);
                if state.accounts.len() == before {
                    return failure(404, "connection_not_found", "no such account");
                }
                ok(&json!({"connectionId": id, "status": "removed"}))
            }
        }
    }
}

fn ok(body: &Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

fn failure(status: u16, code: &str, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({"code": code, "message": message}))
}

fn search(state: &State, body: &Value) -> ResponseTemplate {
    let words: Vec<String> = body["queries"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|q| q["useCase"].as_str())
        .flat_map(|q| {
            q.split_whitespace()
                .map(str::to_lowercase)
                .collect::<Vec<_>>()
        })
        .filter(|w| w.len() > 2)
        .collect();
    let mut schemas = serde_json::Map::new();
    for tool in &state.tools {
        let text = format!("{} {} {}", tool.connector, tool.tool, tool.description).to_lowercase();
        if words.iter().any(|w| text.contains(w.as_str())) {
            schemas.insert(tool.tool.clone(), json!(tool));
        }
    }
    let response = ok(&json!({"results": [], "schemas": schemas}));
    match state.search_delay {
        Some(delay) => response.set_delay(delay),
        None => response,
    }
}

fn schemas(state: &State, body: &Value) -> ResponseTemplate {
    let mut schemas = serde_json::Map::new();
    for slug in body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if let Some(tool) = state.tools.iter().find(|t| t.tool == slug) {
            schemas.insert(slug.to_owned(), json!(tool));
        }
    }
    ok(&json!({"schemas": schemas}))
}

fn execute(state: &mut State, body: &Value) -> ResponseTemplate {
    let call = &body["call"];
    state.executed.push(call.clone());
    let connector = call["connector"].as_str().unwrap_or_default();
    let slug = call["tool"].as_str().unwrap_or_default();
    if !state
        .tools
        .iter()
        .any(|t| t.connector == connector && t.tool == slug)
    {
        return failure(404, "TOOL_NOT_FOUND", "no such tool");
    }
    let alias = call["arguments"]["connector_alias"].as_str();
    let active = state.active(connector);
    let Some(account) = active
        .iter()
        .find(|a| alias.is_none_or(|x| a.alias.as_deref() == Some(x)))
    else {
        return ResponseTemplate::new(409).set_body_json(json!({
            "code": "CONNECTION_REQUIRED", "message": "connect an account",
            "connector": connector, "connectUrl": format!("https://connect.example/{connector}")
        }));
    };
    ok(&json!({"result": {"connector": connector, "tool": slug,
        "data": {"ok": true, "tool": slug, "arguments": call["arguments"]},
        "account": account.alias}}))
}

fn connect(state: &mut State, body: &Value) -> ResponseTemplate {
    let alias = body["alias"].as_str();
    let repaired = body["connectionId"].as_str();
    let mut results = Vec::new();
    for connector in body["connectors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !state.connectors.iter().any(|c| c == connector) {
            results.push(json!({"connector": connector, "status": "failed", "statusReason": "unknown connector"}));
            continue;
        }
        let mut label = format!("{connector}-user@example.org");
        if let Some(old) =
            repaired.and_then(|id| state.accounts.iter_mut().find(|a| a.connection_id == id))
        {
            old.disabled = true;
            label.clone_from(&old.label);
        }
        let id = state.add(connector, alias, &label, "pending");
        results.push(
            json!({"connector": connector, "status": "initiated", "alias": alias,
            "connectionId": id, "connectUrl": format!("https://connect.example/{connector}/{id}"),
            "reinitiated": repaired.is_some()}),
        );
    }
    ok(&json!({"results": results}))
}

fn rename(state: &mut State, id: &str, body: &Value) -> ResponseTemplate {
    if !state.rename_supported {
        return failure(405, "method_not_allowed", "not supported");
    }
    let Some(account) = state.accounts.iter_mut().find(|a| a.connection_id == id) else {
        return failure(404, "connection_not_found", "no such account");
    };
    account.alias = body["alias"].as_str().map(str::to_owned);
    ok(&json!(account))
}
