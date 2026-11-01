//! `prometheus prompt size` (spec: agent-turn-loop, Offline prompt size
//! report): builds the layered system prompt and the tool array exactly
//! as a new session on the given surface would, without any model call or
//! credential, and reports bytes and estimated tokens per layer and per
//! toolset.

use std::fmt::Write as _;

use jiff::Timestamp;
use pmt_agent::prompt::size::{SizeReport, ToolSchema, report};
use pmt_agent::prompt::{self, PromptInputs, platform};
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_providers::ProviderRegistry;
use pmt_providers::metadata::Catalog;
use pmt_tools::{Selection, SurfaceInputs, ToolSurface};
use serde::Serialize;
use serde_json::Value;

use crate::cli::{PromptCommand, PromptSizeArgs};
use crate::context::Context;
use crate::output::{document, print_raw};
use crate::tools_cmd::{Inventory, with_inventory};

/// Surfaces whose sessions run without a person to ask.
const UNATTENDED_PLATFORMS: &[&str] = &["cron", "api"];

/// Runs one `prompt` subcommand.
///
/// # Errors
///
/// The command's failure; the caller prints it and exits with its code.
pub fn run(command: &PromptCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        PromptCommand::Size(args) => size(args, ctx),
    }
}

/// The `prometheus.prompt_size.v1` body.
#[derive(Debug, Serialize)]
struct Body<'a> {
    profile: String,
    platform: &'a str,
    provider: String,
    model: String,
    context_window: u64,
    #[serde(flatten)]
    report: SizeReport,
}

fn size(args: &PromptSizeArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    if platform::builtin(&args.platform).is_none() {
        let names: Vec<&str> = platform::names().collect();
        return Err(Error::new(
            &codes::PROMPT_UNKNOWN_PLATFORM,
            format!(
                "unknown platform '{}' (valid: {})",
                args.platform,
                names.join(", ")
            ),
        ));
    }
    let scope = ctx.scope()?;
    with_inventory(ctx, async |inventory| {
        let body = measure(args, &scope, &inventory).await?;
        if ctx.json {
            return document("prompt size", &body);
        }
        print_raw(&render(&body))
    })
}

async fn measure<'a>(
    args: &'a PromptSizeArgs,
    scope: &pmt_config::ProfileScope,
    inventory: &Inventory<'_>,
) -> Result<Body<'a>, Error> {
    let config = scope.config();
    let (provider, model) = route(args, config)?;
    let (meta, _) = Catalog::new(config, None).metadata(&provider, &model);
    let enabled = if args.toolsets.is_empty() {
        config.tools.enabled_toolsets.clone()
    } else {
        args.toolsets.clone()
    };
    let mut selection = Selection::resolve(
        inventory.registry,
        &enabled,
        &config.tools.disabled_toolsets,
    )?;
    // Cron jobs and API callers have nobody to answer `clarify`.
    if UNATTENDED_PLATFORMS.contains(&args.platform.as_str()) {
        selection = selection.without_interactive(inventory.registry);
    }
    let surface = ToolSurface::build(SurfaceInputs {
        registry: inventory.registry,
        selection: &selection,
        capabilities: inventory.capabilities,
        config: inventory.config,
        probes: inventory.probes,
    })
    .await;
    let built = prompt::build(&PromptInputs {
        config: config.clone(),
        profile_dir: scope.dir().to_path_buf(),
        platform: args.platform.clone(),
        registered_hint: None,
        cwd: std::env::current_dir().ok(),
        no_context_files: false,
        model: model.clone(),
        context_window: Some(meta.context_window),
        tool_names: surface.names().map(str::to_owned).collect(),
        caller_system: None,
        skills_index: None,
        pinned_skills: None,
        memory: None,
        plugin_sections: Vec::new(),
        session_id: SessionId::new(),
        pass_session_id: false,
        started_at: Timestamp::now(),
    })?;
    let schemas = schemas(inventory, surface.serialized());
    Ok(Body {
        profile: scope.name().to_string(),
        platform: &args.platform,
        provider,
        model,
        context_window: meta.context_window,
        report: report(&built.prompt, &schemas, surface.serialized()),
    })
}

/// The provider and model a new session would use.
fn route(args: &PromptSizeArgs, config: &pmt_config::Config) -> Result<(String, String), Error> {
    let registry = ProviderRegistry::for_profile(config, Vec::new())?;
    let current = config.model.provider.as_deref();
    let reference = args.model.as_deref().or(config.model.default.as_deref());
    let Some(reference) = reference else {
        return Ok((current.unwrap_or("auto").to_owned(), "unknown".to_owned()));
    };
    let target = pmt_providers::target::parse(reference, &config.model, &registry, current)?;
    let model = target.model.unwrap_or_else(|| reference.to_owned());
    Ok((target.provider, model))
}

/// Each exposed tool's schema text, with its toolset.
fn schemas(inventory: &Inventory<'_>, serialized: &str) -> Vec<ToolSchema> {
    let tools: Vec<Value> = serde_json::from_str(serialized).unwrap_or_default();
    tools
        .iter()
        .map(|tool| {
            let name = tool["name"].as_str().unwrap_or_default().to_owned();
            let toolset = inventory
                .registry
                .get(&name)
                .map(|t| t.descriptor.toolset.clone())
                .unwrap_or_default();
            ToolSchema {
                name,
                toolset,
                json: tool.to_string(),
            }
        })
        .collect()
}

fn render(body: &Body<'_>) -> String {
    let r = &body.report;
    let mut text = format!(
        "prompt size: {} on {} ({}/{}, window {} tokens)\n\nlayer             tier      bytes   tokens\n",
        body.profile, body.platform, body.provider, body.model, body.context_window
    );
    for layer in &r.layers {
        let _ = writeln!(
            text,
            "{:<17} {:<8} {:>7} {:>8}",
            layer.layer, layer.tier, layer.bytes, layer.tokens
        );
    }
    text.push_str("\ntoolset           tools     bytes   tokens\n");
    for set in &r.toolsets {
        let _ = writeln!(
            text,
            "{:<17} {:<8} {:>7} {:>8}",
            set.toolset,
            set.tools.len(),
            set.bytes,
            set.tokens
        );
    }
    let _ = write!(
        text,
        "\nsystem {} bytes + tools {} bytes = {} bytes, about {} tokens\n",
        r.system_bytes, r.tools_bytes, r.total_bytes, r.total_tokens
    );
    text
}
