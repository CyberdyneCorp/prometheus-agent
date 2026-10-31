//! The listings as data (the `--json` documents) and as text.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::PathBuf;

use pmt_config::schema::{ApprovalMode, Config};
use pmt_core::capability::Capability;
use pmt_core::error::Error;
use pmt_core::secret::Redactor;
use pmt_store::tool_surface::SurfaceTool;
use pmt_tools::budget::ResultBudget;
use pmt_tools::descriptor::{Effect, PathClaim, ResourceClaims};
use pmt_tools::protected::BUILTIN_FILES;
use pmt_tools::{RegisteredTool, ToolRegistry, ToolSurface};
use serde::Serialize;
use serde_json::Value;

use super::{Inventory, unknown_toolset};

/// What `tools list` and `toolsets show` print for an exposed tool.
const EXPOSED: &str = "exposed";
/// A toolset member this binary does not register (yet).
const UNREGISTERED: &str = "unregistered";

/// One `tools list` row: the shape a session records, so the live listing
/// and `--session` print the same columns.
pub type ToolRow = SurfaceTool;

/// Every registered tool with its exposure verdict for the profile's
/// configured toolsets.
///
/// # Errors
///
/// An unknown toolset in the configuration (exit 78).
pub async fn list(inv: &Inventory<'_>) -> Result<Vec<ToolRow>, Error> {
    let (_, surface) = inv.surface(&inv.config.tools.enabled_toolsets).await?;
    Ok(surface_rows(inv.registry, &surface))
}

/// Every tool of `registry` with its verdict in `surface`.
#[must_use]
pub fn surface_rows(registry: &ToolRegistry, surface: &ToolSurface) -> Vec<ToolRow> {
    registry
        .tools()
        .map(|tool| ToolRow {
            name: tool.name().to_owned(),
            toolset: tool.descriptor.toolset.clone(),
            owner: tool.owner.to_string(),
            exposure: exposure(surface, tool.name()),
        })
        .collect()
}

fn exposure(surface: &ToolSurface, name: &str) -> String {
    if surface.get(name).is_some() {
        return EXPOSED.to_owned();
    }
    surface
        .excluded()
        .get(name)
        .map_or_else(|| UNREGISTERED.to_owned(), ToString::to_string)
}

/// `tools list` as text.
#[must_use]
pub fn render_list(rows: &[ToolRow]) -> String {
    if rows.is_empty() {
        return "no tools are registered\n".to_owned();
    }
    let name = width(rows.iter().map(|r| r.name.as_str()), "TOOL");
    let toolset = width(rows.iter().map(|r| r.toolset.as_str()), "TOOLSET");
    let mut out = format!("{:name$}  {:toolset$}  EXPOSURE\n", "TOOL", "TOOLSET");
    for row in rows {
        let _ = writeln!(
            out,
            "{:name$}  {:toolset$}  {}",
            row.name, row.toolset, row.exposure
        );
    }
    out
}

fn width<'a>(values: impl Iterator<Item = &'a str>, header: &str) -> usize {
    values
        .map(str::len)
        .chain([header.len()])
        .max()
        .unwrap_or(0)
}

/// `tools explain <name>`.
#[derive(Debug, Clone, Serialize)]
pub struct Explanation {
    /// Tool name.
    pub name: String,
    /// Its toolset.
    pub toolset: String,
    /// Who registered it.
    pub owner: String,
    /// Model-facing description.
    pub description: String,
    /// Argument schema.
    pub parameters: Value,
    /// Declared capabilities.
    pub capabilities: Vec<String>,
    /// What it can change.
    pub effect: Effect,
    /// Whether repeating a call has no further effect.
    pub idempotent: bool,
    /// Resource claims.
    pub claims: ResourceClaims,
    /// Timeout in seconds.
    pub timeout_secs: u64,
    /// Characters kept inline before spilling; `None`: never spilled.
    pub result_budget_chars: Option<usize>,
    /// `exposed` or the exclusion reason for the configured toolsets.
    pub exposure: String,
    /// `approvals.mode`.
    pub approval_mode: ApprovalMode,
    /// The approval rules that apply to its calls.
    pub approval_rules: Vec<String>,
    /// The backend serving a web tool (`web_search`, `web_extract`), or why
    /// none resolves.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
}

/// Explains `name`.
///
/// # Errors
///
/// `tool.unknown` (exit 2) `unknown tool: <name>`; an unknown configured
/// toolset (exit 78).
pub async fn explain(inv: &Inventory<'_>, name: &str) -> Result<Explanation, Error> {
    let tool = inv.registry.get(name)?;
    let (_, surface) = inv.surface(&inv.config.tools.enabled_toolsets).await?;
    let d = &tool.descriptor;
    let budget = ResultBudget::new(
        inv.config.tools.result_budget.clone(),
        None,
        PathBuf::new(),
        Redactor::default(),
    );
    Ok(Explanation {
        name: d.name.clone(),
        toolset: d.toolset.clone(),
        owner: tool.owner.to_string(),
        description: d.description.clone(),
        parameters: d.parameters.clone(),
        capabilities: d.capabilities.iter().map(ToString::to_string).collect(),
        effect: d.effect,
        idempotent: d.idempotent,
        claims: d.claims.clone(),
        timeout_secs: d
            .timeout
            .map_or(inv.config.tools.default_timeout_secs, |t| t.as_secs()),
        result_budget_chars: budget.threshold(Some(tool)),
        exposure: exposure(&surface, name),
        approval_mode: inv.config.approvals.mode,
        approval_rules: approval_rules(tool, inv.config),
        backend: web_backend(inv, name),
    })
}

/// The resolved backend of a web tool, or `unavailable:<reason>`.
fn web_backend(inv: &Inventory<'_>, name: &str) -> Option<String> {
    let capability = match name {
        "web_search" => pmt_tools::web::Capability::Search,
        "web_extract" => pmt_tools::web::Capability::Extract,
        _ => return None,
    };
    let has_secret = inv.registry.secret_presence();
    Some(
        match pmt_tools::web::resolve(&inv.config.web, capability, has_secret.as_ref()) {
            Ok(id) => id.to_owned(),
            Err(why) => format!("unavailable:{why}"),
        },
    )
}

/// The approval rules that apply to calls of `tool`.
fn approval_rules(tool: &RegisteredTool, config: &Config) -> Vec<String> {
    let mut rules = Vec::new();
    let spawns = tool
        .descriptor
        .capabilities
        .iter()
        .any(|c| matches!(c, Capability::ProcessSpawn(_)));
    if spawns {
        rules.extend([
            "hardline commands (recursive delete of / or the home, mkfs, raw device writes, fork bombs, kill -1, shutdown) are always blocked".to_owned(),
            format!(
                "{} deny rule(s) in approvals.deny always block",
                config.approvals.deny.len()
            ),
            "approvals.mode = \"off\" or --yolo skips the remaining checks".to_owned(),
            "allow rules in grants.toml [approvals].allow skip the prompt".to_owned(),
            "dangerous patterns and the secret-upload and invisible-characters detectors ask; cron, single-query and unattended runs answer from approvals.<context>_mode".to_owned(),
        ]);
    }
    if !tool.descriptor.claims.writes.is_empty() && config.security.protected_instruction_files {
        rules.push(format!(
            "writes to protected instruction files ({}, .prometheus/...) ask in every mode",
            BUILTIN_FILES.join(", ")
        ));
    }
    if !tool.descriptor.claims.writes.is_empty() {
        rules.push("writes to ~/.ssh/config ask in every mode".to_owned());
    }
    if rules.is_empty() {
        rules.push("no approval rule applies; calls run once the tool is exposed".to_owned());
    }
    rules
}

fn effect_name(effect: Effect) -> &'static str {
    match effect {
        Effect::None => "none",
        Effect::Workspace => "workspace",
        Effect::External => "external",
    }
}

fn mode_name(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Manual => "manual",
        ApprovalMode::Smart => "smart",
        ApprovalMode::Off => "off",
    }
}

fn list_or_dash(items: &[String]) -> String {
    if items.is_empty() {
        "-".to_owned()
    } else {
        items.join(", ")
    }
}

fn claims_text(claims: &ResourceClaims) -> String {
    let paths = |list: &[PathClaim]| -> String {
        let items: Vec<String> = list
            .iter()
            .map(|claim| match claim {
                PathClaim::Argument(pointer) => format!("argument {pointer}"),
                PathClaim::Fixed(path) => path.clone(),
                PathClaim::PatchText(pointer) => format!("files of the patch in {pointer}"),
            })
            .collect();
        list_or_dash(&items)
    };
    format!(
        "reads: {}; writes: {}; exclusive: {}; interactive: {}",
        paths(&claims.reads),
        paths(&claims.writes),
        list_or_dash(&claims.exclusive),
        claims.interactive
    )
}

/// `tools explain` as text.
#[must_use]
pub fn render_explain(e: &Explanation) -> String {
    let claims = claims_text(&e.claims);
    let budget = e
        .result_budget_chars
        .map_or_else(|| "never spilled".to_owned(), |c| format!("{c} chars"));
    let mut out = format!("{}\n  {}\n", e.name, e.description);
    for (key, value) in [
        ("toolset", e.toolset.clone()),
        ("owner", e.owner.clone()),
        ("capabilities", list_or_dash(&e.capabilities)),
        ("effect", effect_name(e.effect).to_owned()),
        ("idempotent", e.idempotent.to_string()),
        ("claims", claims),
        ("timeout", format!("{} s", e.timeout_secs)),
        ("result_budget", budget),
        ("exposure", e.exposure.clone()),
        ("approval_mode", mode_name(e.approval_mode).to_owned()),
    ] {
        let _ = writeln!(out, "{key} = {value}");
    }
    if let Some(backend) = &e.backend {
        let _ = writeln!(out, "backend = {backend}");
    }
    out.push_str("approval rules:\n");
    for rule in &e.approval_rules {
        let _ = writeln!(out, "  - {rule}");
    }
    let schema = serde_json::to_string_pretty(&e.parameters).unwrap_or_default();
    let _ = writeln!(out, "schema:\n{schema}");
    out
}

/// One toolset of `toolsets list` and `/toolsets`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolsetRow {
    /// Toolset name.
    pub name: String,
    /// One line.
    pub description: String,
    /// Who registered it.
    pub owner: String,
    /// Tools listed directly or registered under it.
    pub tools: Vec<String>,
    /// Included toolsets.
    pub includes: Vec<String>,
    /// Tools it resolves to, includes expanded.
    pub resolved: usize,
    /// Exposed only when named explicitly.
    pub default_off: bool,
    /// Selects the coding posture.
    pub posture: bool,
    /// Selected in the current session or surface.
    pub active: bool,
}

/// Every toolset of `registry`, `active` marking the selected ones.
///
/// # Errors
///
/// A registry inconsistency (a programming error).
pub fn toolset_rows(
    registry: &ToolRegistry,
    active: &BTreeSet<String>,
) -> Result<Vec<ToolsetRow>, Error> {
    registry
        .toolsets()
        .map(|t| {
            let mut tools = t.tools.clone();
            for tool in registry.tools() {
                if tool.descriptor.toolset == t.name && !tools.iter().any(|n| n == tool.name()) {
                    tools.push(tool.name().to_owned());
                }
            }
            Ok(ToolsetRow {
                name: t.name.clone(),
                description: t.description.clone(),
                owner: t.owner.to_string(),
                tools,
                includes: t.includes.clone(),
                resolved: registry.members(&t.name)?.len(),
                default_off: t.default_off,
                posture: t.posture,
                active: active.contains(&t.name),
            })
        })
        .collect()
}

/// `toolsets list` and `/toolsets` as text.
#[must_use]
pub fn render_toolsets(rows: &[ToolsetRow]) -> String {
    let mut out = String::new();
    for row in rows {
        let flags: Vec<&str> = [
            (row.active, "active"),
            (row.default_off, "default-off"),
            (row.posture, "posture"),
        ]
        .iter()
        .filter_map(|&(on, flag)| on.then_some(flag))
        .collect();
        let count = match row.resolved {
            1 => "1 tool".to_owned(),
            n => format!("{n} tools"),
        };
        let mark = if row.active { '*' } else { ' ' };
        let _ = write!(out, "{mark} {}  ({count})", row.name);
        if !flags.is_empty() {
            let _ = write!(out, "  [{}]", flags.join(", "));
        }
        let _ = writeln!(out, "\n    {}", row.description);
        if !row.tools.is_empty() {
            let _ = writeln!(out, "    tools: {}", row.tools.join(", "));
        }
        if !row.includes.is_empty() {
            let _ = writeln!(out, "    includes: {}", row.includes.join(", "));
        }
    }
    out
}

/// One resolved tool of `toolsets show`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolAvailability {
    /// Tool name.
    pub name: String,
    /// `exposed`, a reason code, or `unregistered`.
    pub availability: String,
}

/// `toolsets show <name>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolsetShow {
    /// Toolset name.
    pub name: String,
    /// One line.
    pub description: String,
    /// Exposed only when named explicitly.
    pub default_off: bool,
    /// Selects the coding posture.
    pub posture: bool,
    /// Included toolsets.
    pub includes: Vec<String>,
    /// Resolved tools in order.
    pub tools: Vec<ToolAvailability>,
}

/// The tools `name` resolves to, with what a session selecting it would
/// do with each.
///
/// # Errors
///
/// `toolset.unknown` (exit 2) naming the closest toolsets.
pub async fn show(inv: &Inventory<'_>, name: &str) -> Result<ToolsetShow, Error> {
    let toolset = inv
        .registry
        .toolset(name)
        .ok_or_else(|| unknown_toolset(inv.registry, name))?;
    let (_, surface) = inv.surface(&[name.to_owned()]).await?;
    let tools = inv
        .registry
        .members(name)?
        .into_iter()
        .map(|tool| ToolAvailability {
            availability: exposure(&surface, &tool),
            name: tool,
        })
        .collect();
    Ok(ToolsetShow {
        name: toolset.name.clone(),
        description: toolset.description.clone(),
        default_off: toolset.default_off,
        posture: toolset.posture,
        includes: toolset.includes.clone(),
        tools,
    })
}

/// `toolsets show` as text.
#[must_use]
pub fn render_show(shown: &ToolsetShow) -> String {
    let mut out = format!("{}: {}\n", shown.name, shown.description);
    if shown.default_off {
        out.push_str("default-off: exposed only when named explicitly\n");
    }
    if shown.posture {
        out.push_str("posture: also selects the coding prompt posture\n");
    }
    if !shown.includes.is_empty() {
        let _ = writeln!(out, "includes: {}", shown.includes.join(", "));
    }
    let width = width(shown.tools.iter().map(|t| t.name.as_str()), "TOOL");
    let _ = writeln!(out, "{:width$}  AVAILABILITY", "TOOL");
    for tool in &shown.tools {
        let _ = writeln!(out, "{:width$}  {}", tool.name, tool.availability);
    }
    out
}
