//! `prometheus tools summary [--json]` (spec: tool-system, Tool
//! configuration summary and post-setup): per session surface, the
//! toolsets its sessions start from, the resolved provider of each
//! provider-backed category its tools use and every selected tool that is
//! not available, with its reason. Surfaces are the fixed ones, the
//! webhook platform and every messaging platform the profile enables or
//! saved a toolset list for.

use std::fmt::Write as _;

use pmt_config::Config;
use pmt_core::error::Error;
use pmt_tools::SessionSurface;
use pmt_tools::surface::Exclusion;
use pmt_tools::web::Capability;
use serde::Serialize;

use super::Inventory;

/// One surface of the summary.
#[derive(Debug, Clone, Serialize)]
pub struct SurfaceSummary {
    /// The surface key (`cli`, `gateway.telegram`...).
    pub surface: String,
    /// The toolsets its sessions start from.
    pub toolsets: Vec<String>,
    /// The toolsets `tools.disabled_toolsets` subtracts.
    pub disabled: Vec<String>,
    /// The provider of each category its tools use.
    pub providers: Vec<ProviderSummary>,
    /// Selected tools that are not exposed, with the reason code.
    pub unavailable: Vec<Unavailable>,
}

/// The resolved provider of one category.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderSummary {
    /// The category (`web_search`, `browser`...).
    pub category: String,
    /// The provider serving it (`None`: nothing does).
    pub provider: Option<String>,
    /// Why nothing does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A selected tool that is not exposed.
#[derive(Debug, Clone, Serialize)]
pub struct Unavailable {
    /// The tool.
    pub tool: String,
    /// Its reason code (`unavailable:<reason>`, `missing_capability:...`).
    pub reason: String,
}

/// The tools each provider-backed category serves.
const CATEGORY_TOOLS: &[(&str, &[&str])] = &[
    ("web_search", &["web_search"]),
    ("web_extract", &["web_extract"]),
    (
        "browser",
        &[
            "browser_exec",
            "browser_cdp",
            "browser_dialog",
            "browser_vault",
        ],
    ),
    ("image_generation", &["image_generate"]),
    ("tts", &["text_to_speech"]),
    ("video", &["video_analyze", "video_generate"]),
    ("x_search", &["x_search"]),
];

/// Answers whether a secret is present, and which web backend an enabled
/// plugin provides.
pub struct Lookups<'a> {
    /// Whether the profile holds a secret.
    pub has_secret: &'a (dyn Fn(&str) -> bool + Send + Sync),
    /// The plugin providing web backend `id` for a capability, if any.
    pub plugin_web: &'a dyn Fn(Capability, &str) -> Option<String>,
}

/// The surfaces the summary covers for `config`.
#[must_use]
pub fn surfaces(config: &Config) -> Vec<SessionSurface> {
    let mut keys: Vec<String> = pmt_config::schema::SurfaceToolsets::KEYS
        .iter()
        .map(|k| (*k).to_owned())
        .collect();
    keys.push(format!("gateway.{}", pmt_tools::selection::WEBHOOK));
    let platforms = config
        .gateway
        .platforms
        .iter()
        .filter(|(_, table)| table.enabled)
        .map(|(name, _)| name.to_lowercase())
        .chain(config.toolsets.surfaces.gateway.keys().cloned());
    for platform in platforms {
        let key = format!("gateway.{platform}");
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys.iter()
        .filter_map(|k| SessionSurface::parse(k))
        .collect()
}

/// Whether sessions of `surface` have someone to answer prompts.
#[must_use]
pub fn attended(surface: &SessionSurface) -> bool {
    !matches!(
        surface,
        SessionSurface::Cron | SessionSurface::Api | SessionSurface::AcpUnattended
    )
}

/// The summary of every surface.
///
/// # Errors
///
/// An unknown toolset in the configuration (exit 78).
pub async fn summarize(
    inv: &Inventory<'_>,
    lookups: &Lookups<'_>,
) -> Result<Vec<SurfaceSummary>, Error> {
    let config = inv.config;
    let mut out = Vec::new();
    for surface in surfaces(config) {
        let mut selection = inv.selection_for(&surface)?;
        if !attended(&surface) {
            selection = selection.without_interactive(inv.registry);
        }
        let built = inv.build(&selection).await;
        let unavailable = selection
            .tools
            .iter()
            .filter_map(|tool| match built.excluded().get(tool) {
                None | Some(Exclusion::NotInToolset | Exclusion::Disabled) => None,
                Some(reason) => Some(Unavailable {
                    tool: tool.clone(),
                    reason: reason.to_string(),
                }),
            })
            .collect();
        let providers = CATEGORY_TOOLS
            .iter()
            .filter(|(_, tools)| tools.iter().any(|t| selection.contains(t)))
            .map(|(category, _)| provider(category, config, lookups))
            .collect();
        out.push(SurfaceSummary {
            surface: surface.key(),
            toolsets: surface.starting_toolsets(inv.registry, &config.tools, &config.toolsets),
            disabled: config.tools.disabled_toolsets.clone(),
            providers,
            unavailable,
        });
    }
    Ok(out)
}

/// The resolved provider of `category`.
fn provider(category: &str, config: &Config, lookups: &Lookups<'_>) -> ProviderSummary {
    let resolved: Result<String, String> = match category {
        "web_search" => web(config, Capability::Search, lookups),
        "web_extract" => web(config, Capability::Extract, lookups),
        "browser" => Ok(if config.browser.cdp_url.is_some() {
            "cdp".to_owned()
        } else {
            config.browser.provider.clone()
        }),
        _ => Err("no provider in this build".to_owned()),
    };
    match resolved {
        Ok(provider) => ProviderSummary {
            category: category.to_owned(),
            provider: Some(provider),
            reason: None,
        },
        Err(reason) => ProviderSummary {
            category: category.to_owned(),
            provider: None,
            reason: Some(reason),
        },
    }
}

/// The web backend of `capability`: built in, else an enabled plugin's.
fn web(config: &Config, capability: Capability, lookups: &Lookups<'_>) -> Result<String, String> {
    match pmt_tools::web::select::resolve(&config.web, capability, lookups.has_secret) {
        Ok(id) => Ok(id.to_owned()),
        Err(why) => {
            let id = pmt_tools::web::select::configured(&config.web, capability);
            match (lookups.plugin_web)(capability, id) {
                Some(plugin) => Ok(format!("{id} (plugin {plugin})")),
                None => Err(why),
            }
        }
    }
}

/// The plain-text rendering.
#[must_use]
pub fn render(summaries: &[SurfaceSummary]) -> String {
    let mut out = String::new();
    for summary in summaries {
        let _ = writeln!(
            out,
            "{}: toolsets {}",
            summary.surface,
            list(&summary.toolsets)
        );
        if !summary.disabled.is_empty() {
            let _ = writeln!(out, "  disabled: {}", summary.disabled.join(", "));
        }
        for p in &summary.providers {
            let value = match (&p.provider, &p.reason) {
                (Some(provider), _) => provider.clone(),
                (None, Some(reason)) => format!("none ({reason})"),
                (None, None) => "none".to_owned(),
            };
            let _ = writeln!(out, "  provider {}: {value}", p.category);
        }
        for u in &summary.unavailable {
            let _ = writeln!(out, "  {} {}", u.tool, u.reason);
        }
    }
    out
}

fn list(items: &[String]) -> String {
    if items.is_empty() {
        "(none)".to_owned()
    } else {
        items.join(", ")
    }
}
