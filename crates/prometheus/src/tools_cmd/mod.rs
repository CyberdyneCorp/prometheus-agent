//! `prometheus tools list [--session]|explain` and `prometheus toolsets list|show`
//! (spec: tool-system, Tool introspection commands; Toolset listing
//! commands; Exposure requires toolset membership and capability coverage).
//!
//! Both answer from the profile's registry (the built-in tools of this
//! binary and toolsets) and a tool surface built exactly as a session would
//! build it from the profile's config, grants and availability probes, so a
//! reason code printed here is the one a session would apply;
//! `tools list --session` instead reads the surface a session recorded when
//! it built it ([`record_session_surface`]). The
//! `/toolsets` slash command renders the same listing for the session's
//! own selection ([`slash_toolsets`]).

pub mod args;
pub mod configure;
mod render;
pub mod setup;
pub mod summary;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use jiff::Timestamp;
use pmt_config::grants::{GrantsFile, SessionGrants, Subject, baseline};
use pmt_config::{Config, ProfileScope};
use pmt_core::capability::CapabilitySet;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_store::Store;
use pmt_store::tool_surface::RecordedSurface;
use pmt_tools::probe::ProbeCache;
use pmt_tools::{Builtins, RegisteredTool, Selection, SurfaceInputs, ToolRegistry, ToolSurface};

pub use render::{
    Explanation, ToolAvailability, ToolRow, ToolsetRow, ToolsetShow, explain, list, render_explain,
    render_list, render_show, render_toolsets, show, surface_rows, toolset_rows,
};

use crate::cli::{ToolsCommand, ToolsListArgs, ToolsetsCommand, ToolsetsListArgs};
use crate::context::Context;
use crate::model_cmd::block_on;
use crate::output::{document, print_raw};

/// The built-in tools of this binary, their handlers bound to the
/// process-wide session environments; the browser tools' availability
/// follows the runtime store and `daemon.toml` of the home at `home`.
/// Each tool group registers its tools here as it lands; a toolset member
/// that is not registered yet is shown as `unregistered`.
#[must_use]
pub fn builtin_tools(home: &Path) -> Vec<RegisteredTool> {
    let browser = crate::runtimes_cmd::browser::probe(home.to_path_buf());
    let mut tools: Vec<RegisteredTool> =
        pmt_tools::builtin_tools(&pmt_tools::environments::Environments::process())
            .into_iter()
            .map(|tool| {
                if tool.descriptor.toolset == pmt_tools::browser::TOOLSET {
                    tool.with_probe(Arc::clone(&browser))
                } else {
                    tool
                }
            })
            .collect();
    tools.extend(pmt_skills::tools::tools(
        &pmt_skills::SkillSessions::process(),
    ));
    tools.push(pmt_memory::tool::registered());
    tools
}

/// The registry of profile `scope`: this binary's built-in tools, the
/// shipped toolsets and a bundle per platform of an adapter plugin.
///
/// # Errors
///
/// A built-in tool or toolset that does not load (a programming error).
pub fn registry(scope: &ProfileScope) -> Result<ToolRegistry, Error> {
    let plugin_platforms = pmt_config::platforms::plugin_platforms();
    Ok(ToolRegistry::new(
        scope.name().as_str(),
        Arc::new(Builtins::standard(builtin_tools(scope.home()))?),
    )
    .with_platforms(plugin_platforms.iter().map(String::as_str)))
}

/// The tools `names` select (composites expanded), for checking that a
/// chat policy only narrows a profile's toolsets; `Err(name)` names an
/// unknown toolset. The registry is built once, on first use.
///
/// # Errors
///
/// The unknown toolset's name.
pub fn expand_toolsets(names: &[String]) -> Result<std::collections::BTreeSet<String>, String> {
    static REGISTRY: std::sync::OnceLock<Option<ToolRegistry>> = std::sync::OnceLock::new();
    let registry = REGISTRY.get_or_init(|| {
        Builtins::standard(builtin_tools(Path::new("")))
            .ok()
            .map(|builtins| ToolRegistry::new("validation", Arc::new(builtins)))
    });
    let Some(registry) = registry else {
        // Built-ins that do not load are reported by every other command;
        // here the check falls back to accepting the names.
        return Ok(names.iter().cloned().collect());
    };
    let mut tools = std::collections::BTreeSet::new();
    for name in names {
        tools.extend(registry.members(name).map_err(|_| name.clone())?);
    }
    Ok(tools)
}

/// What the listings work on: a registry, the profile's config, the
/// effective capability set of a session of the profile and a probe cache.
pub struct Inventory<'a> {
    /// The profile's registry.
    pub registry: &'a ToolRegistry,
    /// The profile's configuration.
    pub config: &'a Config,
    /// A session's effective capabilities (baseline plus grants).
    pub capabilities: &'a CapabilitySet,
    /// Availability probe cache.
    pub probes: &'a ProbeCache,
}

impl Inventory<'_> {
    /// The surface a session selecting `enabled` (minus the configured
    /// disabled toolsets) would get.
    ///
    /// # Errors
    ///
    /// An unknown toolset name (`registry.unknown_name`, exit 78).
    pub async fn surface(&self, enabled: &[String]) -> Result<(Selection, ToolSurface), Error> {
        let selection =
            Selection::resolve(self.registry, enabled, &self.config.tools.disabled_toolsets)?;
        let surface = self.build(&selection).await;
        Ok((selection, surface))
    }

    /// The surface a session with `selection` would get (schema variants
    /// of an unknown route).
    pub async fn build(&self, selection: &Selection) -> ToolSurface {
        ToolSurface::build(SurfaceInputs {
            registry: self.registry,
            selection,
            capabilities: self.capabilities,
            config: self.config,
            probes: self.probes,
            route: &pmt_tools::variants::RouteTraits::default(),
        })
        .await
    }
}

/// Runs one `tools` subcommand.
///
/// # Errors
///
/// The command's failure; the caller prints it and exits with its code.
pub fn run(command: &ToolsCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        ToolsCommand::List(ToolsListArgs {
            session: Some(reference),
        }) => list_session(reference, ctx),
        ToolsCommand::List(ToolsListArgs { session: None }) => with_inventory(ctx, async |inv| {
            let rows = list(&inv).await?;
            if ctx.json {
                return document("tools list", &serde_json::json!({ "tools": rows }));
            }
            print_raw(&render_list(&rows))
        }),
        ToolsCommand::Explain { name } => with_inventory(ctx, async |inv| {
            let explanation = explain(&inv, name).await?;
            if ctx.json {
                return document("tools explain", &explanation);
            }
            print_raw(&render_explain(&explanation))
        }),
        ToolsCommand::Toolsets(args) => list_toolsets(args, ctx),
        ToolsCommand::Configure { surface } => configure::configure(ctx, surface.as_deref()),
        ToolsCommand::Enable { toolset, surface } => {
            configure::enable(ctx, toolset, true, surface.as_deref())
        }
        ToolsCommand::Disable { toolset, surface } => {
            configure::enable(ctx, toolset, false, surface.as_deref())
        }
        ToolsCommand::Provider { category, provider } => {
            configure::provider(ctx, category, provider)
        }
        ToolsCommand::Summary => summary(ctx),
        ToolsCommand::PostSetup { toolset, yes } => {
            let consent = if *yes {
                setup::Consent::Given
            } else {
                setup::Consent::Ask
            };
            setup::post_setup(ctx, toolset, &consent)
        }
    }
}

/// `tools summary [--json]`.
fn summary(ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let cwd = std::env::current_dir().unwrap_or_default();
    let plugins = crate::plugins_host::for_session(&scope, &cwd).current();
    let secrets = scope.secrets().clone();
    let has_secret = move |name: &str| secrets.resolve(name).is_ok();
    let plugin_web = |capability: pmt_tools::web::Capability, id: &str| {
        let category = match capability {
            pmt_tools::web::Capability::Search => pmt_plugins::providers::Category::WebSearch,
            pmt_tools::web::Capability::Extract => pmt_plugins::providers::Category::WebExtract,
        };
        plugins
            .providers
            .require(category, id)
            .ok()
            .map(|p| p.plugin.clone())
    };
    let lookups = summary::Lookups {
        has_secret: &has_secret,
        plugin_web: &plugin_web,
    };
    with_inventory(ctx, async |inv| {
        let summaries = summary::summarize(&inv, &lookups).await?;
        if ctx.json {
            return document(
                "tools summary",
                &serde_json::json!({ "surfaces": summaries }),
            );
        }
        print_raw(&summary::render(&summaries))
    })
}

/// Records the surface `session` just built, so `tools list --session`
/// reports what the session was given. Session start and every surface
/// rebuild (compaction, `/tools reload --now`) call it.
///
/// # Errors
///
/// `session.not_found`, store errors.
pub fn record_session_surface(
    store: &Store,
    session: SessionId,
    registry: &ToolRegistry,
    surface: &ToolSurface,
) -> Result<(), Error> {
    store.record_tool_surface(
        session,
        &RecordedSurface {
            generation: surface.generation(),
            recorded_at: Timestamp::now(),
            tools: surface_rows(registry, surface),
        },
    )
}

/// `tools list --session`: the surface the session recorded.
fn list_session(reference: &str, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let store = Store::open(&scope)?;
    let id = store.resolve_session(reference)?;
    let recorded = store.tool_surface(id)?.ok_or_else(|| {
        Error::new(
            &codes::TOOL_SURFACE_UNRECORDED,
            format!("session {id} recorded no tool surface"),
        )
        .with_hint("`prometheus tools list` shows the surface a new session would get")
    })?;
    if ctx.json {
        return document(
            "tools list",
            &serde_json::json!({
                "session": id.to_string(),
                "generation": recorded.generation,
                "recorded_at": recorded.recorded_at.to_string(),
                "tools": recorded.tools,
            }),
        );
    }
    print_raw(&format!(
        "session {id} (surface generation {}, built {})\n{}",
        recorded.generation,
        recorded.recorded_at,
        render_list(&recorded.tools)
    ))
}

/// Runs one `toolsets` subcommand.
///
/// # Errors
///
/// The command's failure; `toolset.unknown` (exit 2) for an unknown name.
pub fn toolsets(command: &ToolsetsCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        ToolsetsCommand::List(args) => list_toolsets(args, ctx),
        ToolsetsCommand::Show { name } => with_inventory(ctx, async |inv| {
            let shown = show(&inv, name).await?;
            if ctx.json {
                return document("toolsets show", &shown);
            }
            print_raw(&render_show(&shown))
        }),
    }
}

fn list_toolsets(args: &ToolsetsListArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let config = scope.config();
    let registry = registry(&scope)?;
    let enabled = match &args.surface {
        Some(surface) => configure::check_surface(surface)?.starting_toolsets(
            &registry,
            &config.tools,
            &config.toolsets,
        ),
        None => config.tools.enabled_toolsets.clone(),
    };
    let selection = Selection::resolve(&registry, &enabled, &config.tools.disabled_toolsets)?;
    let rows = toolset_rows(
        &registry,
        &active(&selection, &config.tools.disabled_toolsets),
    )?;
    if ctx.json {
        return document("toolsets list", &serde_json::json!({ "toolsets": rows }));
    }
    print_raw(&render_toolsets(&rows))
}

/// `/toolsets`: the listing with the session's own selection marked
/// active (the typed slash registry registers it by name).
///
/// # Errors
///
/// A registry inconsistency (a programming error).
pub fn slash_toolsets(registry: &ToolRegistry, selection: &Selection) -> Result<String, Error> {
    let rows = toolset_rows(registry, &active(selection, &[]))?;
    Ok(render_toolsets(&rows))
}

/// The selected toolsets that no disabled toolset names.
fn active(selection: &Selection, disabled: &[String]) -> BTreeSet<String> {
    selection
        .toolsets
        .iter()
        .filter(|t| !disabled.contains(t))
        .cloned()
        .collect()
}

/// Builds the inventory of the selected profile and runs `body` on it.
pub(crate) fn with_inventory<F>(ctx: &Context<'_>, body: F) -> Result<u8, Error>
where
    F: AsyncFnOnce(Inventory<'_>) -> Result<u8, Error>,
{
    let scope = ctx.scope()?;
    let config = scope.config();
    let secrets = scope.secrets().clone();
    let registry =
        registry(&scope)?.with_secret_presence(Arc::new(move |name| secrets.resolve(name).is_ok()));
    let grants = SessionGrants::new(baseline(&config.security), GrantsFile::of(scope.profile()));
    let capabilities = grants.capabilities(&Subject::Profile)?;
    let probes = ProbeCache::new();
    let inventory = Inventory {
        registry: &registry,
        config,
        capabilities: &capabilities,
        probes: &probes,
    };
    block_on(body(inventory))?
}

/// `toolset.unknown` (exit 2) with the registry's suggestion text.
pub(crate) fn unknown_toolset(registry: &ToolRegistry, name: &str) -> Error {
    let err = registry.unknown_toolset(name);
    let mut usage = Error::new(&codes::TOOLSET_UNKNOWN, err.message());
    if let Some(hint) = err.hint() {
        usage = usage.with_hint(hint);
    }
    usage
}

#[cfg(test)]
mod tests;
