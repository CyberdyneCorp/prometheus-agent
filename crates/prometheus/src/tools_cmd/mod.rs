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

mod render;

use std::collections::BTreeSet;
use std::sync::Arc;

use jiff::Timestamp;
use pmt_config::Config;
use pmt_config::grants::{GrantsFile, SessionGrants, Subject, baseline};
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

/// The built-in tools of this binary. Each tool group registers its tools
/// here as it lands; a toolset member that is not registered yet is shown
/// as `unregistered`.
#[must_use]
pub fn builtin_tools() -> Vec<RegisteredTool> {
    Vec::new()
}

/// The registry of profile `profile`: this binary's built-in tools and the
/// shipped toolsets.
///
/// # Errors
///
/// A built-in tool or toolset that does not load (a programming error).
pub fn registry(profile: &str) -> Result<ToolRegistry, Error> {
    Ok(ToolRegistry::new(
        profile,
        Arc::new(Builtins::standard(builtin_tools())?),
    ))
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
        let surface = ToolSurface::build(SurfaceInputs {
            registry: self.registry,
            selection: &selection,
            capabilities: self.capabilities,
            config: self.config,
            probes: self.probes,
        })
        .await;
        Ok((selection, surface))
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
    }
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
    let registry = registry(scope.name().as_str())?;
    let enabled = args
        .surface
        .as_ref()
        .and_then(|s| config.toolsets.surfaces.get(s))
        .unwrap_or(&config.tools.enabled_toolsets);
    let selection = Selection::resolve(&registry, enabled, &config.tools.disabled_toolsets)?;
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
fn with_inventory<F>(ctx: &Context<'_>, body: F) -> Result<u8, Error>
where
    F: AsyncFnOnce(Inventory<'_>) -> Result<u8, Error>,
{
    let scope = ctx.scope()?;
    let config = scope.config();
    let registry = registry(scope.name().as_str())?;
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
