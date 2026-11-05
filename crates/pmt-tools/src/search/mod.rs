//! Tool search progressive disclosure (spec: tool-system, Tool search
//! progressive disclosure; Tool search listing, ranking and limits).
//!
//! When a session's deferrable tools (MCP and plugin tools, plus the names
//! in `tools.tool_search.defer`) would take too much of the frozen tool
//! array, they leave it: the model gets `tool_search` (find tools by
//! keywords), `tool_describe` (their schemas) and `tool_call` (run them)
//! instead. The deferred tools stay part of the session's resolved tool
//! set, so a `tool_call` runs through the full dispatch pipeline under the
//! real name; anything outside that set is an unknown tool. The decision
//! and the `tool_search` listing are made once, when the surface freezes.
//!
//! A [`Remote`] (hosted connectors) adds tools that never enter the array
//! nor the listing: `tool_search` asks it on every call and ranks its hits
//! together with the local entries, `tool_describe` asks it for the
//! schemas of its names, and `tool_call` gets its tools from it.

mod manifest;
mod rank;

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use pmt_config::schema::{ToolListing, ToolSearchConfig, ToolSearchMode};
use serde_json::{Map, Value, json};

use crate::descriptor::{
    CallContext, Effect, HandlerFuture, Owner, RegisteredTool, ToolDescriptor, ToolHandler,
};
use crate::registry::ToolRegistry;
use crate::result::ToolResult;
use crate::surface::ToolDefinition;
use crate::variants::RouteTraits;

/// Finds deferred tools.
pub const TOOL_SEARCH: &str = "tool_search";
/// Shows deferred tools' schemas.
pub const TOOL_DESCRIBE: &str = "tool_describe";
/// Runs deferred tools.
pub const TOOL_CALL: &str = "tool_call";
/// The bridge tools' toolset label.
pub const TOOLSET: &str = "tool_search";

/// Tools deferred besides MCP and plugin tools when
/// `tools.tool_search.defer` is unset.
pub const DEFAULT_DEFER: &[&str] = &[
    "computer_use",
    "session_search",
    "image_generate",
    "todo",
    "process",
    "cronjob",
    "browser_vault",
];

/// The most queries one `tool_search` and calls one `tool_call` take.
pub const MAX_BATCH: usize = 10;

/// Characters per token of the size thresholds.
const CHARS_PER_TOKEN: u64 = 4;

/// One deferred tool as the bridge tools describe it.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Tool name.
    pub name: String,
    /// Advertised description.
    pub description: String,
    /// Advertised argument schema.
    pub parameters: Value,
    /// Its server or toolset (`mcp-github`, a plugin id, `core`).
    pub source: String,
}

impl Entry {
    /// The schema's `required` names.
    fn required(&self) -> Vec<String> {
        self.parameters
            .get("required")
            .and_then(Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// What a [`Remote`] answers.
pub type RemoteFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Tools reached only through the bridge, never registered locally
/// (hosted connectors).
pub trait Remote: Send + Sync {
    /// Its hits for `queries`; empty when it cannot answer (a slow or
    /// unreachable remote degrades to local results, never to an error).
    fn search<'a>(&'a self, queries: &'a [String]) -> RemoteFuture<'a, Vec<Entry>>;
    /// The entries of those `names` it knows.
    fn describe<'a>(&'a self, names: &'a [String]) -> RemoteFuture<'a, Vec<Entry>>;
    /// Whether `name` is in its namespace.
    fn owns(&self, name: &str) -> bool;
    /// The tool `tool_call` runs for `name`.
    fn tool(&self, name: &str) -> Option<Arc<RegisteredTool>>;
}

/// The deferred tools of one frozen surface.
#[derive(Clone)]
pub struct Catalog {
    entries: Vec<Entry>,
    default_limit: usize,
    max_limit: usize,
    remote: Option<Arc<dyn Remote>>,
}

impl std::fmt::Debug for Catalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Catalog")
            .field("entries", &self.entries.len())
            .field("remote", &self.remote.is_some())
            .finish_non_exhaustive()
    }
}

impl Catalog {
    /// The entry named `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// Every entry, in surface order.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The remote tools, when the session has some.
    #[must_use]
    pub fn remote(&self) -> Option<&Arc<dyn Remote>> {
        self.remote.as_ref()
    }

    /// `tool_search` over the local entries only.
    #[must_use]
    pub fn search(&self, queries: &[String], limit: Option<u64>) -> Value {
        self.search_with(queries, limit, Vec::new())
    }

    /// `tool_search`: each query ranked on its own over the local entries
    /// and `extra` (remote hits) in one pass, at most `limit` (clamped to
    /// `max_search_limit`) names per query, plus one shared map of every
    /// matched tool's description and required parameters (and `source`
    /// for remote ones).
    #[must_use]
    pub fn search_with(&self, queries: &[String], limit: Option<u64>, extra: Vec<Entry>) -> Value {
        let mut entries = self.entries.clone();
        for entry in extra {
            if entries.iter().all(|e| e.name != entry.name) {
                entries.push(entry);
            }
        }
        let local = self.entries.len();
        let limit = limit
            .map_or(self.default_limit, |l| {
                usize::try_from(l).unwrap_or(usize::MAX)
            })
            .clamp(1, self.max_limit);
        let index = rank::Index::new(&entries);
        let mut matched: BTreeMap<&str, Value> = BTreeMap::new();
        let groups: Vec<Value> = queries
            .iter()
            .map(|query| {
                let hits = index.rank(query, limit);
                let names: Vec<&str> = hits.iter().map(|&i| entries[i].name.as_str()).collect();
                for &i in &hits {
                    let entry = &entries[i];
                    matched.entry(&entry.name).or_insert_with(|| {
                        let mut found =
                            json!({"description": entry.description, "required": entry.required()});
                        if i >= local {
                            found["source"] = Value::String(entry.source.clone());
                        }
                        found
                    });
                }
                let mut group = json!({"query": query, "tools": names});
                if names.is_empty() {
                    group["available_sources"] = self.sources();
                }
                group
            })
            .collect();
        json!({"results": groups, "tools": matched})
    }

    /// `tool_describe`: the schemas of `names`; unknown names are listed
    /// in `not_found`.
    #[must_use]
    pub fn describe(&self, names: &[String]) -> Value {
        self.describe_with(names, &[])
    }

    /// `tool_describe` over the local entries and `remote` ones.
    #[must_use]
    pub fn describe_with(&self, names: &[String], remote: &[Entry]) -> Value {
        let mut tools = Map::new();
        let mut not_found = Vec::new();
        for name in names {
            match self
                .get(name)
                .or_else(|| remote.iter().find(|e| &e.name == name))
            {
                Some(entry) => {
                    tools.insert(
                        name.clone(),
                        json!({"description": entry.description, "parameters": entry.parameters}),
                    );
                }
                None => not_found.push(name.clone()),
            }
        }
        json!({"tools": tools, "not_found": not_found})
    }

    /// Each source with its deferred tool count.
    fn sources(&self) -> Value {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for entry in &self.entries {
            *counts.entry(&entry.source).or_default() += 1;
        }
        json!(counts)
    }
}

/// Whether `tool` may leave the tool array: a name in the defer list, or
/// an MCP or plugin tool that is neither a `core` tool nor surface-gated.
fn deferrable(tool: &RegisteredTool, registry: &ToolRegistry, defer: &[String]) -> bool {
    let name = tool.name();
    if defer.iter().any(|d| d == name) {
        return true;
    }
    if !matches!(tool.owner, Owner::Mcp(_) | Owner::Plugin(_)) {
        return false;
    }
    let core = registry
        .members("core")
        .is_ok_and(|members| members.iter().any(|m| m == name));
    let gated = registry
        .toolset(&tool.descriptor.toolset)
        .is_some_and(|t| t.surface_gated);
    !core && !gated
}

/// What a surface build decided: which tools stay in the array.
pub(crate) struct Split {
    /// Indices (into the candidates) of the deferred tools.
    pub(crate) deferred: Vec<usize>,
}

/// Decides which of `tools` (with their advertised `definitions`) leave
/// the array; `None` when tool search stays off for the session.
pub(crate) fn split(
    tools: &[Arc<RegisteredTool>],
    definitions: &[ToolDefinition],
    registry: &ToolRegistry,
    config: &ToolSearchConfig,
    route: &RouteTraits,
) -> Option<Split> {
    if config.mode == ToolSearchMode::Off {
        return None;
    }
    let defer: Vec<String> = config
        .defer
        .clone()
        .unwrap_or_else(|| DEFAULT_DEFER.iter().map(|&s| s.to_owned()).collect());
    let deferred: Vec<usize> = tools
        .iter()
        .enumerate()
        .filter(|(_, tool)| deferrable(tool, registry, &defer))
        .map(|(i, _)| i)
        .collect();
    let deferrable_own = deferred
        .iter()
        .any(|&i| matches!(tools[i].owner, Owner::Mcp(_) | Owner::Plugin(_)));
    if !deferrable_own {
        // Only built-ins from the defer list: nothing large to hide.
        return None;
    }
    let size: u64 = deferred
        .iter()
        .map(|&i| definition_chars(&definitions[i]))
        .sum();
    let defer_now = match config.mode {
        ToolSearchMode::On => true,
        ToolSearchMode::Auto => size > threshold_chars(config, route.context_window),
        ToolSearchMode::Off => false,
    };
    defer_now.then_some(Split { deferred })
}

/// The smaller of `threshold_pct` of the context window and
/// `listing_max_tokens`, in characters.
fn threshold_chars(config: &ToolSearchConfig, window: Option<u64>) -> u64 {
    let listing = u64::from(config.listing_max_tokens) * CHARS_PER_TOKEN;
    window.map_or(listing, |tokens| {
        (tokens * u64::from(config.threshold_pct) / 100 * CHARS_PER_TOKEN).min(listing)
    })
}

fn definition_chars(definition: &ToolDefinition) -> u64 {
    let chars = definition.name.len()
        + definition.description.len()
        + definition.parameters.to_string().len();
    u64::try_from(chars).unwrap_or(u64::MAX)
}

/// The deferred tools' catalog and the three bridge tools over it.
pub(crate) fn bridge(
    deferred: &[(&Arc<RegisteredTool>, &ToolDefinition)],
    config: &ToolSearchConfig,
) -> (Arc<Catalog>, Vec<RegisteredTool>) {
    let entries: Vec<Entry> = deferred
        .iter()
        .map(|(tool, definition)| Entry {
            name: definition.name.clone(),
            description: definition.description.clone(),
            parameters: definition.parameters.clone(),
            source: tool.descriptor.toolset.clone(),
        })
        .collect();
    bridge_over(entries, config, None)
}

/// The bridge over `entries` (the local deferred tools) and `remote`.
pub(crate) fn bridge_over(
    entries: Vec<Entry>,
    config: &ToolSearchConfig,
    remote: Option<Arc<dyn Remote>>,
) -> (Arc<Catalog>, Vec<RegisteredTool>) {
    let max_limit = usize::try_from(config.max_search_limit)
        .unwrap_or(25)
        .max(1);
    let catalog = Arc::new(Catalog {
        default_limit: usize::try_from(config.search_default_limit)
            .unwrap_or(5)
            .clamp(1, max_limit),
        max_limit,
        entries,
        remote,
    });
    let listing = match config.listing {
        ToolListing::Off => None,
        ToolListing::Auto | ToolListing::On => Some(manifest::render(
            catalog.entries(),
            u64::from(config.listing_max_tokens) * CHARS_PER_TOKEN,
        )),
    };
    let tools = vec![
        search_tool(&catalog, listing.as_deref()),
        describe_tool(&catalog),
        call_tool(),
    ];
    (catalog, tools)
}

fn search_tool(catalog: &Arc<Catalog>, listing: Option<&str>) -> RegisteredTool {
    let mut description = "Search the tools that are not listed directly (MCP servers, plugins and rarely used tools) by keywords. Returns matching tool names per query with their descriptions and required parameters; get a schema with tool_describe and run a tool with tool_call.".to_owned();
    if catalog.remote.is_some() {
        description.push_str(" Hosted app connectors (e-mail, documents, issue trackers...) are searched too; their tools are named connectors__<app>__<tool>.");
    }
    if let Some(listing) = listing.filter(|l| !l.is_empty()) {
        description.push_str("\n\nSearchable tools:\n");
        description.push_str(listing);
    }
    let parameters = json!({
        "type": "object",
        "properties": {
            "queries": {
                "type": "array",
                "items": {"type": "string", "minLength": 1},
                "minItems": 1,
                "maxItems": MAX_BATCH,
                "description": "Keyword queries, each searched on its own"
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "description": "Matches per query"
            }
        },
        "required": ["queries"]
    });
    bridge_tool(
        TOOL_SEARCH,
        description,
        parameters,
        Arc::new(Search(Arc::clone(catalog))),
    )
}

fn describe_tool(catalog: &Arc<Catalog>) -> RegisteredTool {
    let parameters = json!({
        "type": "object",
        "properties": {
            "names": {
                "type": "array",
                "items": {"type": "string"},
                "minItems": 1,
                "description": "Tool names from tool_search"
            }
        },
        "required": ["names"]
    });
    bridge_tool(
        TOOL_DESCRIBE,
        "Show the full description and argument schema of tools found with tool_search.".to_owned(),
        parameters,
        Arc::new(Describe(Arc::clone(catalog))),
    )
}

/// `tool_call`'s entry: the dispatcher runs it (each call goes through
/// the full pipeline); the handler only answers if it is reached anyway.
fn call_tool() -> RegisteredTool {
    let parameters = json!({
        "type": "object",
        "properties": {
            "calls": {
                "type": ["array", "string"],
                "items": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "arguments": {"type": "object"}
                    },
                    "required": ["name"]
                },
                "minItems": 1,
                "maxItems": MAX_BATCH,
                "description": "The tools to run: [{\"name\": ..., \"arguments\": {...}}]"
            }
        },
        "required": ["calls"]
    });
    let mut descriptor = ToolDescriptor::new(
        TOOL_CALL,
        TOOLSET,
        "Run tools found with tool_search, by name with their arguments.",
        parameters,
    );
    descriptor.effect = Effect::External;
    RegisteredTool::new(
        descriptor,
        Owner::Builtin,
        Arc::new(Answer(Arc::new(|_: &Value| {
            Value::String("tool_call is run by the session dispatcher".to_owned())
        }))),
    )
}

fn bridge_tool(
    name: &str,
    description: String,
    parameters: Value,
    handler: Arc<dyn ToolHandler>,
) -> RegisteredTool {
    let mut descriptor = ToolDescriptor::new(name, TOOLSET, description, parameters);
    descriptor.effect = Effect::None;
    descriptor.idempotent = true;
    descriptor.claims = Some(crate::descriptor::ResourceClaims::none());
    RegisteredTool::new(descriptor, Owner::Builtin, handler)
}

/// `tool_search`: the remote's hits (when there is one) ranked with the
/// local entries.
struct Search(Arc<Catalog>);

impl ToolHandler for Search {
    fn call<'a>(&'a self, _: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let queries = strings(&arguments, "queries");
            let limit = arguments.get("limit").and_then(Value::as_u64);
            let extra = match &self.0.remote {
                Some(remote) => remote.search(&queries).await,
                None => Vec::new(),
            };
            Ok(ToolResult::ok(
                self.0.search_with(&queries, limit, extra).to_string(),
            ))
        })
    }
}

/// `tool_describe`: local schemas, and the remote's for its names.
struct Describe(Arc<Catalog>);

impl ToolHandler for Describe {
    fn call<'a>(&'a self, _: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let names = strings(&arguments, "names");
            let remote = match &self.0.remote {
                Some(remote) => {
                    let theirs: Vec<String> = names
                        .iter()
                        .filter(|n| self.0.get(n).is_none() && remote.owns(n))
                        .cloned()
                        .collect();
                    if theirs.is_empty() {
                        Vec::new()
                    } else {
                        remote.describe(&theirs).await
                    }
                }
                None => Vec::new(),
            };
            Ok(ToolResult::ok(
                self.0.describe_with(&names, &remote).to_string(),
            ))
        })
    }
}

/// A handler answering JSON computed from the arguments.
struct Answer(Arc<dyn Fn(&Value) -> Value + Send + Sync>);

impl ToolHandler for Answer {
    fn call<'a>(&'a self, _: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        let value = (self.0)(&arguments);
        Box::pin(async move { Ok(ToolResult::ok(value.to_string())) })
    }
}

fn strings(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}
