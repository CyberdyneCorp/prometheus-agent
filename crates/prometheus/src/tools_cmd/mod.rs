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
use pmt_plugins::host::CallInfo;
use pmt_plugins::tools::SessionCalls;
use pmt_store::Store;
use pmt_store::tool_surface::RecordedSurface;
use pmt_tools::probe::ProbeCache;
use pmt_tools::{
    Builtins, RegisteredTool, Selection, SessionSurface, SurfaceInputs, ToolRegistry, ToolSurface,
};

pub use render::{
    Explanation, ToolAvailability, ToolRow, ToolsetRow, ToolsetShow, explain, list, render_explain,
    render_list, render_show, render_toolsets, show, surface_rows, toolset_rows,
};

use crate::cli::{ToolsCommand, ToolsListArgs, ToolsetsCommand, ToolsetsListArgs};
use crate::context::Context;
use crate::model_cmd::block_on;
use crate::output::{document, print_raw};

/// The built-in tools of this binary, their handlers bound to the
/// process-wide session environments; the local browser tools'
/// availability follows the runtime store and `daemon.toml` of the home at
/// `home` (`browser_cdp` and `browser_dialog` follow `browser.cdp_url`; a
/// session's `/browser connect` opts it in itself, and
/// `browser_close_profile` follows `browser.real_profile_autoclose`).
/// Each tool group registers its tools here as it lands; a toolset member
/// that is not registered yet is shown as `unregistered`.
#[must_use]
pub fn builtin_tools(home: &Path) -> Vec<RegisteredTool> {
    let browser = crate::runtimes_cmd::browser::probe(home.to_path_buf());
    let mut tools: Vec<RegisteredTool> =
        pmt_tools::builtin_tools(&pmt_tools::environments::Environments::process())
            .into_iter()
            .map(|tool| {
                // Tools with their own probe (the connected-browser tools,
                // `browser_close_profile`) need no local runtime.
                if tool.descriptor.toolset == pmt_tools::browser::TOOLSET && tool.probe.is_none() {
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
    tools.push(crate::connectors_host::registered_tool());
    tools.extend(pmt_media::tools(&pmt_media::MediaSessions::process()));
    tools
}

/// The registry of profile `scope` as a session started now in this
/// process's working directory gets it ([`extended_registry`] over the
/// profile's current plugin set and MCP servers; no server is started).
///
/// # Errors
///
/// A built-in tool or toolset that does not load (a programming error).
pub fn registry(scope: &ProfileScope) -> Result<ToolRegistry, Error> {
    profile_registry(scope).map(|(registry, _)| registry)
}

/// [`registry`] and the plugin and MCP toolsets a session opts in.
///
/// # Errors
///
/// As [`registry`].
pub fn profile_registry(scope: &ProfileScope) -> Result<(ToolRegistry, Vec<String>), Error> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let plugins = crate::plugins_host::for_session(scope, &cwd);
    let mcp = crate::mcp_host::manager(scope);
    let at = cwd.to_string_lossy().into_owned();
    // No tool runs from a listing; a call would report the CLI surface.
    let calls: SessionCalls = Arc::new(move |session: &str| CallInfo {
        session_id: (!session.is_empty()).then(|| session.to_owned()),
        surface: "cli".to_owned(),
        cwd: Some(at.clone()),
    });
    extended_registry(scope, &plugins, &mcp, &calls)
}

/// The one way a registry with extensions is built, for sessions and the
/// listings alike: this binary's built-in tools, the shipped toolsets, a
/// bundle per platform of an adapter plugin, the tools of `plugins`'
/// current set and those `mcp`'s servers listed last. Also answers the
/// plugin and MCP toolsets (see [`opt_in`]). `calls` resolves what each
/// plugin tool call reports as its session.
///
/// # Errors
///
/// As [`registry`].
pub fn extended_registry(
    scope: &ProfileScope,
    plugins: &crate::plugins_host::ProfilePlugins,
    mcp: &pmt_mcp::Manager,
    calls: &SessionCalls,
) -> Result<(ToolRegistry, Vec<String>), Error> {
    let mut registry = base_registry(scope)?;
    // Listing the tools runs for no session.
    let mut toolsets = plugins
        .current()
        .register_tools(&mut registry, &calls(""), calls);
    toolsets.extend(mcp.register(&mut registry));
    // The memory provider's tools join the `memory` toolset itself.
    crate::memory_provider::tools::register(scope, plugins, &mut registry);
    crate::context_engine::register_tools(scope, plugins, &mut registry);
    Ok((registry, toolsets))
}

/// Adds the plugin and MCP toolsets `extensions` to `enabled` as a session
/// on `surface` does: enabling a plugin or configuring a server opts its
/// tools in, except for webhook-triggered runs; a platform bundle never
/// opts in.
pub fn opt_in(enabled: &mut Vec<String>, extensions: &[String], surface: &SessionSurface) {
    if surface.is_webhook() {
        return;
    }
    for toolset in extensions {
        if !enabled.contains(toolset) && !pmt_config::platforms::is_known(toolset) {
            enabled.push(toolset.clone());
        }
    }
}

/// [`registry`] without plugin or MCP tools.
///
/// # Errors
///
/// As [`registry`].
pub fn base_registry(scope: &ProfileScope) -> Result<ToolRegistry, Error> {
    let plugin_platforms = pmt_config::platforms::plugin_platforms();
    Ok(ToolRegistry::new(
        scope.name().as_str(),
        Arc::new(Builtins::standard(builtin_tools(scope.home()))?),
    )
    .with_platforms(plugin_platforms.iter().map(String::as_str))
    .with_secret_presence(presence(scope)))
}

/// The web and media categories of `scope` as `tools explain` and `doctor`
/// show them.
#[must_use]
pub fn media_categories(
    scope: &ProfileScope,
    registry: &ToolRegistry,
) -> Option<Vec<pmt_media::explain::CategoryView>> {
    let launcher = crate::chat::session::media::launcher(scope);
    let setup = pmt_media::explain::inspection_setup(
        crate::chat::session::media::credentials(scope),
        scope.dir(),
        Some(launcher),
    )?;
    let presence = registry.secret_presence();
    Some(pmt_media::explain::categories(setup, presence.as_ref()))
}

/// Which credentials the profile holds, as availability probes ask: a
/// secret by name, or `signin:<provider>` for a usable sign-in (the tool
/// gateway's self-hosted token stands for the Nous sign-in).
#[must_use]
pub fn presence(scope: &ProfileScope) -> pmt_tools::probe::SecretPresence {
    let secrets = scope.secrets().clone();
    let gateway_token = scope.config().tool_gateway.token_secret.clone();
    Arc::new(move |name: &str| {
        let Some(provider) = name.strip_prefix("signin:") else {
            return secrets.resolve(name).is_ok();
        };
        if provider == "nous"
            && gateway_token
                .as_ref()
                .is_some_and(|t| secrets.resolve(&t.0).is_ok())
        {
            return true;
        }
        let extras = pmt_providers::auth::Credentials::new(secrets.store().clone());
        pmt_providers::resolve::CredentialExtras::subscription(&extras, provider)
            .ok()
            .flatten()
            .is_some()
    })
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
    /// The plugin and MCP toolsets a session opts in ([`opt_in`]).
    pub extensions: &'a [String],
    /// The profile's configuration.
    pub config: &'a Config,
    /// A session's effective capabilities (baseline plus grants).
    pub capabilities: &'a CapabilitySet,
    /// Availability probe cache.
    pub probes: &'a ProbeCache,
    /// The profile (web and media categories); `None` in listings built
    /// without one.
    pub scope: Option<&'a ProfileScope>,
}

impl Inventory<'_> {
    /// `enabled` with the plugin and MCP toolsets a session on `surface`
    /// opts in.
    #[must_use]
    pub fn opted_in(&self, mut enabled: Vec<String>, surface: &SessionSurface) -> Vec<String> {
        opt_in(&mut enabled, self.extensions, surface);
        enabled
    }

    /// The selection a new session on `surface` starts with (before
    /// dropping interactive tools on unattended surfaces).
    ///
    /// # Errors
    ///
    /// An unknown toolset name (`registry.unknown_name`, exit 78).
    pub fn selection_for(&self, surface: &SessionSurface) -> Result<Selection, Error> {
        let starting =
            surface.starting_toolsets(self.registry, &self.config.tools, &self.config.toolsets);
        Selection::resolve(
            self.registry,
            &self.opted_in(starting, surface),
            &self.config.tools.disabled_toolsets,
        )
    }

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
    let (registry, extensions) = profile_registry(&scope)?;
    let surface = args
        .surface
        .as_deref()
        .map(configure::check_surface)
        .transpose()?;
    let mut enabled = match &surface {
        Some(surface) => surface.starting_toolsets(&registry, &config.tools, &config.toolsets),
        None => config.tools.enabled_toolsets.clone(),
    };
    opt_in(
        &mut enabled,
        &extensions,
        surface.as_ref().unwrap_or(&SessionSurface::Cli),
    );
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
/// The profile's MCP servers are started first, as for a new session, so
/// the listings see what a session would be offered.
pub(crate) fn with_inventory<F>(ctx: &Context<'_>, body: F) -> Result<u8, Error>
where
    F: AsyncFnOnce(Inventory<'_>) -> Result<u8, Error>,
{
    let scope = ctx.scope()?;
    let config = scope.config();
    block_on(async {
        crate::mcp_host::discover(&scope, false).await;
        let (registry, extensions) = profile_registry(&scope)?;
        let grants =
            SessionGrants::new(baseline(&config.security), GrantsFile::of(scope.profile()));
        let capabilities = grants.capabilities(&Subject::Profile)?;
        let probes = ProbeCache::new();
        let inventory = Inventory {
            scope: Some(&scope),
            registry: &registry,
            extensions: &extensions,
            config,
            capabilities: &capabilities,
            probes: &probes,
        };
        body(inventory).await
    })?
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
