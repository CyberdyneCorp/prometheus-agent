//! Connector tools behind the tool-search bridge (spec: connectors, Remote
//! connector tools through the search bridge; Connector calls run through
//! the dispatch pipeline; Multiple accounts per connector).
//!
//! Nothing here is registered: `tool_search` asks the gateway on every
//! call (bounded, degrading to local results), the hits are remembered so
//! `tool_describe` and `tool_call` know their vendor slugs, and a
//! `tool_call` entry becomes a [`RegisteredTool`] made for that call,
//! which the dispatcher runs through the whole pipeline under its
//! `connectors__` name.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};

use super::client::{Gateway, GatewayError};
use super::names::{self, ConnectorName};
use super::wire::{Account, ToolSchema};
use crate::descriptor::{
    CallContext, Effect, HandlerFuture, Owner, RegisteredTool, ToolDescriptor, ToolHandler,
};
use crate::result::ToolResult;
use crate::search::{Entry, Remote, RemoteFuture};

/// The argument naming the account a call runs under.
pub const ALIAS_ARGUMENT: &str = "connector_alias";

/// One remembered remote tool.
#[derive(Debug, Clone)]
struct Known {
    connector: String,
    slug: String,
    description: String,
    schema: Value,
}

/// The connectors of one session.
pub struct Connectors {
    gateway: Gateway,
    known: Mutex<BTreeMap<String, Known>>,
}

impl std::fmt::Debug for Connectors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connectors")
            .field("gateway", &self.gateway)
            .finish_non_exhaustive()
    }
}

impl Connectors {
    /// The connectors reached through `gateway`.
    #[must_use]
    pub fn new(gateway: Gateway) -> Arc<Self> {
        Arc::new(Self {
            gateway,
            known: Mutex::new(BTreeMap::new()),
        })
    }

    fn remember(&self, name: &str, known: Known) {
        self.known
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name.to_owned(), known);
    }

    fn recall(&self, name: &str) -> Option<Known> {
        self.known
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned()
    }

    /// Remembers `schemas` under their model-facing names; when two compose
    /// to one name the conventional slug wins and the other is dropped
    /// with a WARN.
    fn absorb(&self, schemas: Vec<ToolSchema>) -> Vec<String> {
        let mut chosen: BTreeMap<String, (bool, ToolSchema)> = BTreeMap::new();
        for schema in schemas {
            let (name, conventional) = names::compose(&schema.connector, &schema.tool);
            match chosen.get(&name) {
                Some((kept, other)) if *kept || !conventional => {
                    tracing::warn!(
                        name,
                        kept = other.tool,
                        dropped = schema.tool,
                        "connector tool name collision"
                    );
                }
                Some((_, other)) => {
                    tracing::warn!(
                        name,
                        kept = schema.tool,
                        dropped = other.tool,
                        "connector tool name collision"
                    );
                    chosen.insert(name, (conventional, schema));
                }
                None => {
                    chosen.insert(name, (conventional, schema));
                }
            }
        }
        chosen
            .into_iter()
            .map(|(name, (_, schema))| {
                self.remember(
                    &name,
                    Known {
                        connector: schema.connector,
                        slug: schema.tool,
                        description: schema.description,
                        schema: schema.input_schema,
                    },
                );
                name
            })
            .collect()
    }

    /// The entries of remembered `found` names, with the account argument
    /// added where a connector has several active accounts.
    fn entries(&self, found: &[String], accounts: &[Account]) -> Vec<Entry> {
        found
            .iter()
            .filter_map(|name| {
                let known = self.recall(name)?;
                let (description, parameters) = with_accounts(
                    &known.description,
                    &known.schema,
                    &active(accounts, &known.connector),
                );
                Some(Entry {
                    name: name.clone(),
                    description,
                    parameters,
                    source: names::TOOLSET.to_owned(),
                })
            })
            .collect()
    }

    /// The accounts, or none when the gateway cannot list them.
    async fn accounts_or_none(&self) -> Vec<Account> {
        self.gateway.accounts().await.unwrap_or_else(|err| {
            tracing::debug!("connector accounts not listed: {err}");
            Vec::new()
        })
    }

    async fn search_entries(&self, queries: &[String]) -> Result<Vec<Entry>, GatewayError> {
        let found = self.absorb(self.gateway.search(queries).await?);
        if found.is_empty() {
            return Ok(Vec::new());
        }
        let accounts = self.accounts_or_none().await;
        Ok(self.entries(&found, &accounts))
    }

    async fn describe_entries(&self, names: &[String]) -> Vec<Entry> {
        let mut found = Vec::new();
        for name in names {
            if self.recall(name).is_some() {
                found.push(name.clone());
                continue;
            }
            let Some(parsed) = names::parse(name) else {
                continue;
            };
            for slug in parsed.slugs() {
                match self.gateway.schemas(std::slice::from_ref(&slug)).await {
                    Ok(schemas) if !schemas.is_empty() => {
                        if let Some(schema) = schemas.into_iter().next() {
                            self.remember(
                                name,
                                Known {
                                    connector: schema.connector,
                                    slug: schema.tool,
                                    description: schema.description,
                                    schema: schema.input_schema,
                                },
                            );
                            found.push(name.clone());
                        }
                        break;
                    }
                    Ok(_) => {}
                    Err(err) => {
                        tracing::debug!(name, "connector schema not fetched: {err}");
                        break;
                    }
                }
            }
        }
        if found.is_empty() {
            return Vec::new();
        }
        let accounts = self.accounts_or_none().await;
        self.entries(&found, &accounts)
    }
}

impl Remote for Connectors {
    fn search<'a>(&'a self, queries: &'a [String]) -> RemoteFuture<'a, Vec<Entry>> {
        Box::pin(async move {
            match self.search_entries(queries).await {
                Ok(entries) => entries,
                Err(err) => {
                    tracing::info!("connector search skipped, local results only: {err}");
                    Vec::new()
                }
            }
        })
    }

    fn describe<'a>(&'a self, names: &'a [String]) -> RemoteFuture<'a, Vec<Entry>> {
        Box::pin(self.describe_entries(names))
    }

    fn owns(&self, name: &str) -> bool {
        names::is_connector(name)
    }

    fn tool(&self, name: &str) -> Option<Arc<RegisteredTool>> {
        let parsed = names::parse(name)?;
        let known = self.recall(name);
        let (description, parameters) = known.as_ref().map_or_else(
            || {
                (
                    format!("{} tool {}", parsed.connector, parsed.tool),
                    json!({"type": "object"}),
                )
            },
            |k| (k.description.clone(), k.schema.clone()),
        );
        let parameters = with_alias_argument(&parameters);
        let mut descriptor = ToolDescriptor::new(name, names::TOOLSET, description, parameters);
        descriptor.effect = Effect::External;
        descriptor.idempotent = false;
        let handler = Call {
            gateway: self.gateway.clone(),
            slugs: known.map_or_else(|| parsed.slugs(), |k| vec![k.slug]),
            name: parsed,
        };
        Some(Arc::new(
            RegisteredTool::new(
                descriptor,
                Owner::Capability(names::TOOLSET.to_owned()),
                Arc::new(handler),
            )
            .with_provenance(json!({
                "source": format!("connector:{}", handler_connector(name)),
                "trust": "untrusted",
            })),
        ))
    }
}

fn handler_connector(name: &str) -> String {
    names::parse(name).map(|n| n.connector).unwrap_or_default()
}

/// The active accounts of `connector`.
fn active<'a>(accounts: &'a [Account], connector: &str) -> Vec<&'a Account> {
    accounts
        .iter()
        .filter(|a| a.connector == connector && a.is_active())
        .collect()
}

/// `description` and `schema` with the account argument when `active` has
/// two or more accounts: `Accounts: a, b.` in the description, and
/// `connector_alias` (an enum of the aliases) required when every account
/// is named.
fn with_accounts(description: &str, schema: &Value, active: &[&Account]) -> (String, Value) {
    let mut schema = open_schema(schema);
    if active.len() < 2 {
        return (description.to_owned(), schema);
    }
    let aliases: Vec<&str> = active.iter().filter_map(|a| a.alias.as_deref()).collect();
    let shown: Vec<&str> = active.iter().map(|a| a.display_name()).collect();
    let description = format!("{description} Accounts: {}.", shown.join(", "));
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties.insert(
            ALIAS_ARGUMENT.to_owned(),
            json!({"type": "string", "enum": aliases, "description": "The account to use"}),
        );
    }
    if aliases.len() == active.len()
        && let Some(object) = schema.as_object_mut()
    {
        let required = object.entry("required").or_insert_with(|| json!([]));
        if let Some(list) = required.as_array_mut() {
            list.push(json!(ALIAS_ARGUMENT));
        }
    }
    (description, schema)
}

/// `schema` accepting `connector_alias` (whether the call needs it is
/// decided per call, from the accounts).
fn with_alias_argument(schema: &Value) -> Value {
    let mut schema = open_schema(schema);
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties
            .entry(ALIAS_ARGUMENT)
            .or_insert_with(|| json!({"type": "string", "description": "The account to use"}));
    }
    schema
}

/// `schema` as an object schema with a `properties` map.
fn open_schema(schema: &Value) -> Value {
    let mut schema = if schema.is_object() {
        schema.clone()
    } else {
        json!({"type": "object"})
    };
    if let Some(object) = schema.as_object_mut() {
        object.entry("type").or_insert_with(|| json!("object"));
        object.entry("properties").or_insert_with(|| json!({}));
    }
    schema
}

/// One connector tool call.
struct Call {
    gateway: Gateway,
    name: ConnectorName,
    slugs: Vec<String>,
}

impl ToolHandler for Call {
    fn call<'a>(&'a self, _: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move { Ok(self.run(arguments).await) })
    }
}

impl Call {
    async fn run(&self, arguments: Value) -> ToolResult {
        let connector = &self.name.connector;
        if let Err(message) = self.check_account(&arguments).await {
            return ToolResult::error(message);
        }
        let mut last = None;
        for slug in &self.slugs {
            match self.gateway.execute(connector, slug, &arguments).await {
                Ok(done) => {
                    let alias = done.account.or_else(|| {
                        arguments
                            .get(ALIAS_ARGUMENT)
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    });
                    return ToolResult::ok(answer(done.data, alias));
                }
                Err(err) if err.code() == Some("TOOL_NOT_FOUND") => last = Some(err),
                Err(err) => return failed(connector, &err),
            }
        }
        last.map_or_else(
            || {
                ToolResult::error(format!(
                    "unknown tool: connectors__{connector}__{}",
                    self.name.tool
                ))
            },
            |err| failed(connector, &err),
        )
    }

    /// With two or more active accounts, `connector_alias` must name one
    /// of them (and is required when every one is named).
    async fn check_account(&self, arguments: &Value) -> Result<(), String> {
        let Ok(accounts) = self.gateway.accounts().await else {
            return Ok(());
        };
        let active = active(&accounts, &self.name.connector);
        if active.len() < 2 {
            return Ok(());
        }
        let aliases: Vec<&str> = active.iter().filter_map(|a| a.alias.as_deref()).collect();
        let given = arguments.get(ALIAS_ARGUMENT).and_then(Value::as_str);
        let listed = aliases.join(", ");
        match given {
            Some(alias) if aliases.contains(&alias) => Ok(()),
            Some(alias) => Err(format!(
                "{} has no account named '{alias}'; set {ALIAS_ARGUMENT} to one of: {listed}",
                self.name.connector
            )),
            None if aliases.len() == active.len() => Err(format!(
                "{} has several accounts; set {ALIAS_ARGUMENT} to one of: {listed}",
                self.name.connector
            )),
            None => Ok(()),
        }
    }
}

/// The result text: the data, under `data` beside `account` when the
/// account is named.
fn answer(data: Value, account: Option<String>) -> String {
    match account {
        Some(account) => json!({"account": account, "data": data}).to_string(),
        None => match data {
            Value::String(text) => text,
            other => other.to_string(),
        },
    }
}

/// A failed call's result; `CONNECTION_REQUIRED` points at
/// `manage_connections`.
fn failed(connector: &str, err: &GatewayError) -> ToolResult {
    let code = err.code().unwrap_or("GATEWAY_ERROR");
    let mut body = json!({"error": code, "connector": connector});
    if code == "CONNECTION_REQUIRED" {
        body["message"] = json!(format!(
            "no {connector} account is linked; ask the user, then call manage_connections with action \"connect\" and connectors [\"{connector}\"]"
        ));
    } else {
        body["message"] = json!(err.to_string());
    }
    ToolResult::error(body.to_string())
}
