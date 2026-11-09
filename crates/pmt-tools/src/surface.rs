//! A session's tool surface: what the model may call, frozen at session
//! start (spec: tool-system, Exposure requires toolset membership and
//! capability coverage; Availability probes are cached per profile with
//! flake grace; Tool surface is frozen for the session; Descriptions
//! reference only present tools).
//!
//! A registered tool is exposed when the session's selection includes it,
//! the session's capability set covers every capability it declares, and
//! its availability probe passes (or the session opted into it itself,
//! [`Selection::session_opt_ins`]). Every other registered tool is reported
//! with exactly one [`Exclusion`]. The resulting [`ToolSurface`] is
//! immutable: its definitions (names, descriptions, schemas, order) are
//! serialized once and sent byte-identical on every model call, and later
//! registry, credential or config changes reach only surfaces built
//! afterwards (the next session, a `--now` rebuild or a compaction).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use pmt_config::schema::Config;
use pmt_core::capability::CapabilitySet;
use serde::Serialize;
use serde_json::Value;

use crate::descriptor::{ExposedNames, RegisteredTool};
use crate::probe::{ProbeCache, ProbeContext};
use crate::registry::ToolRegistry;
use crate::search;
use crate::selection::Selection;
use crate::variants::RouteTraits;

/// Why a registered tool is not exposed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exclusion {
    /// No selected toolset includes it.
    NotInToolset,
    /// A disabled toolset removed it.
    Disabled,
    /// The session lacks a capability it declares.
    MissingCapability(String),
    /// Its availability probe failed.
    Unavailable(String),
    /// A tool it needs is not selected (`browser_exec` without `terminal`).
    Requires(String),
}

impl fmt::Display for Exclusion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInToolset => f.write_str("not_in_toolset"),
            Self::Disabled => f.write_str("disabled"),
            Self::MissingCapability(cap) => write!(f, "missing_capability:{cap}"),
            Self::Unavailable(reason) => write!(f, "unavailable:{reason}"),
            Self::Requires(tool) => write!(f, "requires:{tool}"),
        }
    }
}

/// One tool definition as sent to the model.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolDefinition {
    /// Tool name.
    pub name: String,
    /// Description after rewriting for this session.
    pub description: String,
    /// Argument schema.
    pub parameters: Value,
}

/// What a surface build needs.
#[derive(Debug, Clone, Copy)]
pub struct SurfaceInputs<'a> {
    /// The profile's registry.
    pub registry: &'a ToolRegistry,
    /// The session's toolset selection.
    pub selection: &'a Selection,
    /// The session's effective capability set.
    pub capabilities: &'a CapabilitySet,
    /// The profile's configuration (probes read it).
    pub config: &'a Config,
    /// The process-wide probe cache.
    pub probes: &'a ProbeCache,
    /// The session's main route, which picks schema variants.
    pub route: &'a RouteTraits,
}

/// The frozen tool surface of one session.
#[derive(Debug, Clone)]
pub struct ToolSurface {
    generation: u64,
    tools: Vec<Arc<RegisteredTool>>,
    definitions: Vec<ToolDefinition>,
    serialized: Arc<str>,
    excluded: BTreeMap<String, Exclusion>,
    posture: bool,
    variants: BTreeMap<String, &'static str>,
    /// Tools left out of the array behind the tool-search bridge, still
    /// part of the session's resolved tool set.
    deferred: Vec<Arc<RegisteredTool>>,
    catalog: Option<Arc<search::Catalog>>,
}

impl ToolSurface {
    /// Builds the surface: exposure verdicts, probe verdicts (each distinct
    /// probe at most once), description rewriting, serialization.
    pub async fn build(inputs: SurfaceInputs<'_>) -> Self {
        let SurfaceInputs {
            registry,
            selection,
            capabilities,
            ..
        } = inputs;
        let mut excluded = BTreeMap::new();
        let mut candidates = Vec::new();
        for tool in registry.tools() {
            match static_exclusion(tool, selection, capabilities) {
                Some(reason) => {
                    excluded.insert(tool.name().to_owned(), reason);
                }
                None => candidates.push(Arc::clone(tool)),
            }
        }
        let verdicts = probe_all(&candidates, inputs).await;
        candidates.retain(|tool| {
            let failure = probe_failure(tool, &verdicts).or_else(|| {
                let check = tool.route_check.as_ref()?;
                check(inputs.config, inputs.route).err()
            });
            match failure {
                Some(_) if selection.session_opt_ins.contains(tool.name()) => true,
                Some(reason) => {
                    excluded.insert(tool.name().to_owned(), Exclusion::Unavailable(reason));
                    false
                }
                None => true,
            }
        });
        // Model order: the selection's expansion order.
        let position: BTreeMap<&str, usize> = selection
            .tools
            .iter()
            .enumerate()
            .map(|(i, name)| (name.as_str(), i))
            .collect();
        candidates.sort_by_key(|tool| position.get(tool.name()).copied().unwrap_or(usize::MAX));
        Self::freeze(registry, candidates, excluded, selection.posture, inputs)
    }

    fn freeze(
        registry: &ToolRegistry,
        tools: Vec<Arc<RegisteredTool>>,
        excluded: BTreeMap<String, Exclusion>,
        posture: bool,
        inputs: SurfaceInputs<'_>,
    ) -> Self {
        let exposed: ExposedNames = tools.iter().map(|t| t.name().to_owned()).collect();
        let absent: BTreeSet<&str> = excluded.keys().map(String::as_str).collect();
        let mut variants = BTreeMap::new();
        let mut definitions: Vec<ToolDefinition> = tools
            .iter()
            .map(|tool| {
                let advertised = tool.advertised(inputs.config, inputs.route);
                if let Some(variant) = advertised.variant {
                    variants.insert(tool.name().to_owned(), variant);
                }
                let description = match &tool.rewriter {
                    Some(rewrite) => rewrite(&advertised.description, &exposed),
                    None => advertised.description,
                };
                warn_on_absent_names(tool.name(), &description, &absent);
                ToolDefinition {
                    name: tool.name().to_owned(),
                    description,
                    parameters: advertised.parameters,
                }
            })
            .collect();
        let mut tools = tools;
        let (deferred, catalog) = defer(
            registry,
            &mut tools,
            &mut definitions,
            &inputs.config.tools.tool_search,
            inputs.route,
        );
        let serialized = serde_json::to_string(&definitions)
            .unwrap_or_else(|_| "[]".to_owned())
            .into();
        Self {
            generation: registry.generation(),
            tools,
            definitions,
            serialized,
            excluded,
            posture,
            variants,
            deferred,
            catalog,
        }
    }

    /// The schema variant `tool` advertises in this surface (`None`: it is
    /// not exposed or has no variants).
    #[must_use]
    pub fn variant(&self, tool: &str) -> Option<&'static str> {
        self.variants.get(tool).copied()
    }

    /// The registry generation the surface was built from.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The exposed tool named `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Arc<RegisteredTool>> {
        self.tools.iter().find(|t| t.name() == name)
    }

    /// The tool named `name` in the session's resolved tool set: exposed,
    /// or deferred behind the tool-search bridge.
    #[must_use]
    pub fn resolve(&self, name: &str) -> Option<&Arc<RegisteredTool>> {
        self.get(name)
            .or_else(|| self.deferred.iter().find(|t| t.name() == name))
    }

    /// The tools deferred behind the tool-search bridge, in model order.
    #[must_use]
    pub fn deferred(&self) -> &[Arc<RegisteredTool>] {
        &self.deferred
    }

    /// The deferred tools' catalog, when the session uses tool search.
    #[must_use]
    pub fn catalog(&self) -> Option<&Arc<search::Catalog>> {
        self.catalog.as_ref()
    }

    /// Adds `remote`'s tools behind the tool-search bridge (spec:
    /// connectors, Remote connector tools through the search bridge): the
    /// bridge tools are rebuilt over the same local entries (or appear,
    /// with none, when nothing was deferred) and nothing of `remote` enters
    /// the array. The bridge is offered whatever `tools.tool_search.mode`
    /// says, as remote tools have no other way in.
    #[must_use]
    pub fn with_remote(
        mut self,
        remote: Arc<dyn search::Remote>,
        config: &pmt_config::schema::ToolSearchConfig,
    ) -> Self {
        let entries = match self.catalog.take() {
            Some(catalog) => {
                let bridge = [
                    search::TOOL_SEARCH,
                    search::TOOL_DESCRIBE,
                    search::TOOL_CALL,
                ];
                self.tools.retain(|t| !bridge.contains(&t.name()));
                self.definitions
                    .retain(|d| !bridge.contains(&d.name.as_str()));
                catalog.entries().to_vec()
            }
            None => Vec::new(),
        };
        let (catalog, bridge) = search::bridge_over(entries, config, Some(remote));
        for tool in bridge {
            self.definitions.push(ToolDefinition {
                name: tool.name().to_owned(),
                description: tool.descriptor.description.clone(),
                parameters: tool.descriptor.parameters.clone(),
            });
            self.tools.push(Arc::new(tool));
        }
        self.serialized = serde_json::to_string(&self.definitions)
            .unwrap_or_else(|_| "[]".to_owned())
            .into();
        self.catalog = Some(catalog);
        self
    }

    /// The remote tool `tool_call` runs for `name`, when a remote owns it.
    #[must_use]
    pub fn remote_tool(&self, name: &str) -> Option<Arc<RegisteredTool>> {
        let remote = self.catalog.as_ref()?.remote()?;
        if remote.owns(name) {
            remote.tool(name)
        } else {
            None
        }
    }

    /// Exposed tool names, in model order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.iter().map(|t| t.name())
    }

    /// The definitions sent to the model, in order.
    #[must_use]
    pub fn definitions(&self) -> &[ToolDefinition] {
        &self.definitions
    }

    /// The definitions as the JSON array sent on every model call.
    #[must_use]
    pub fn serialized(&self) -> &str {
        &self.serialized
    }

    /// Every registered tool that is not exposed, with its reason.
    #[must_use]
    pub fn excluded(&self) -> &BTreeMap<String, Exclusion> {
        &self.excluded
    }

    /// Whether the session selected a posture toolset (`coding`).
    #[must_use]
    pub fn posture(&self) -> bool {
        self.posture
    }
}

/// Moves the deferrable tools out of `tools` and `definitions` and appends
/// the bridge tools when tool search is active for the session.
fn defer(
    registry: &ToolRegistry,
    tools: &mut Vec<Arc<RegisteredTool>>,
    definitions: &mut Vec<ToolDefinition>,
    config: &pmt_config::schema::ToolSearchConfig,
    route: &RouteTraits,
) -> (Vec<Arc<RegisteredTool>>, Option<Arc<search::Catalog>>) {
    let Some(split) = search::split(tools, definitions, registry, config, route) else {
        return (Vec::new(), None);
    };
    let mut kept_tools = Vec::new();
    let mut kept_definitions = Vec::new();
    let mut deferred = Vec::new();
    let mut deferred_definitions = Vec::new();
    for (i, (tool, definition)) in tools.drain(..).zip(definitions.drain(..)).enumerate() {
        if split.deferred.contains(&i) {
            deferred.push(tool);
            deferred_definitions.push(definition);
        } else {
            kept_tools.push(tool);
            kept_definitions.push(definition);
        }
    }
    let pairs: Vec<_> = deferred.iter().zip(deferred_definitions.iter()).collect();
    let (catalog, bridge) = search::bridge(&pairs, config);
    for tool in bridge {
        kept_definitions.push(ToolDefinition {
            name: tool.name().to_owned(),
            description: tool.descriptor.description.clone(),
            parameters: tool.descriptor.parameters.clone(),
        });
        kept_tools.push(Arc::new(tool));
    }
    *tools = kept_tools;
    *definitions = kept_definitions;
    (deferred, Some(catalog))
}

/// The first reason found without probing: toolset membership, then the
/// disabled list, then capability coverage, then required companions.
fn static_exclusion(
    tool: &RegisteredTool,
    selection: &Selection,
    capabilities: &CapabilitySet,
) -> Option<Exclusion> {
    let name = tool.name();
    if selection.disabled.contains(name) {
        return Some(Exclusion::Disabled);
    }
    if !selection.contains(name) {
        return Some(Exclusion::NotInToolset);
    }
    if let Some(cap) = tool
        .descriptor
        .capabilities
        .iter()
        .find(|cap| !capabilities.allows(cap))
    {
        return Some(Exclusion::MissingCapability(cap.to_string()));
    }
    tool.descriptor
        .requires
        .iter()
        .find(|needed| !selection.contains(needed))
        .map(|needed| Exclusion::Requires(needed.clone()))
}

/// Runs each distinct probe of `tools` once, concurrently.
async fn probe_all(
    tools: &[Arc<RegisteredTool>],
    inputs: SurfaceInputs<'_>,
) -> BTreeMap<String, Result<(), String>> {
    let mut distinct = BTreeMap::new();
    for probe in tools.iter().filter_map(|t| t.probe.as_ref()) {
        distinct
            .entry(probe.id().to_owned())
            .or_insert_with(|| Arc::clone(probe));
    }
    let has_secret = inputs.registry.secret_presence();
    let context = ProbeContext {
        profile: inputs.registry.profile(),
        config: inputs.config,
        has_secret: has_secret.as_ref(),
    };
    let checks = distinct.iter().map(|(id, probe)| async move {
        (
            id.clone(),
            inputs.probes.verdict(probe.as_ref(), context).await,
        )
    });
    join_all(checks).await.into_iter().collect()
}

/// Awaits every future concurrently, keeping their order.
async fn join_all<F: Future>(futures: impl Iterator<Item = F>) -> Vec<F::Output> {
    let mut pending: Vec<std::pin::Pin<Box<F>>> = futures.map(Box::pin).collect();
    let mut outputs: Vec<Option<F::Output>> = pending.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut done = true;
        for (future, output) in pending.iter_mut().zip(outputs.iter_mut()) {
            if output.is_none() {
                match future.as_mut().poll(cx) {
                    std::task::Poll::Ready(value) => *output = Some(value),
                    std::task::Poll::Pending => done = false,
                }
            }
        }
        if done {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
    outputs.into_iter().flatten().collect()
}

fn probe_failure(
    tool: &RegisteredTool,
    verdicts: &BTreeMap<String, Result<(), String>>,
) -> Option<String> {
    let probe = tool.probe.as_ref()?;
    verdicts.get(probe.id())?.clone().err()
}

/// Logs a description that names a registered tool absent from the
/// session (a static description should never name another toolset's
/// tool; a rewriter must drop it).
fn warn_on_absent_names(tool: &str, description: &str, absent: &BTreeSet<&str>) {
    for word in description.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        if absent.contains(word) {
            tracing::error!(
                tool,
                absent = word,
                "tool description names a tool absent from the session"
            );
        }
    }
}

#[cfg(test)]
mod tests;
