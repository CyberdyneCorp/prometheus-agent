//! Toolset expansion table (spec: tool-system, Core toolset membership;
//! Single-purpose toolsets and default-off toolsets; Composite toolsets and
//! the coding posture): every shipped toolset resolves to exactly its
//! specified tools, and the built surfaces of the spec scenarios hold
//! exactly what they should.

use std::sync::Arc;

use pmt_config::schema::Config;
use pmt_core::capability::{BASELINE, CapabilitySet};
use pmt_tools::descriptor::{CallContext, HandlerFuture, Owner, ToolHandler};
use pmt_tools::probe::ProbeCache;
use pmt_tools::toolset::{CORE, builtin, builtin_tool_names};
use pmt_tools::{
    Builtins, RegisteredTool, Selection, SurfaceInputs, ToolDescriptor, ToolRegistry, ToolResult,
    ToolSurface,
};
use serde_json::{Value, json};

struct Ok;

impl ToolHandler for Ok {
    fn call<'a>(&'a self, _: &'a CallContext, _: Value) -> HandlerFuture<'a> {
        Box::pin(async { Result::Ok(ToolResult::ok("ok")) })
    }
}

/// Every shipped tool name, registered under the first toolset listing it.
fn registry() -> ToolRegistry {
    let toolsets = builtin();
    let tools = builtin_tool_names()
        .into_iter()
        .map(|name| {
            let home = toolsets
                .iter()
                .find(|t| t.name != "core" && t.tools.contains(&name))
                .map_or("core", |t| t.name.as_str());
            let descriptor = ToolDescriptor::new(&name, home, "A tool.", json!({"type": "object"}));
            RegisteredTool::new(descriptor, Owner::Builtin, Arc::new(Ok))
        })
        .collect();
    ToolRegistry::new("default", Arc::new(Builtins::standard(tools).unwrap()))
}

fn strings(names: &[&str]) -> Vec<String> {
    names.iter().map(|&n| n.to_owned()).collect()
}

fn resolve(enabled: &[&str], disabled: &[&str]) -> Selection {
    Selection::resolve(&registry(), &strings(enabled), &strings(disabled)).unwrap()
}

async fn surface(enabled: &[&str], disabled: &[&str]) -> ToolSurface {
    let registry = registry();
    let selection = Selection::resolve(&registry, &strings(enabled), &strings(disabled)).unwrap();
    ToolSurface::build(SurfaceInputs {
        registry: &registry,
        selection: &selection,
        capabilities: &CapabilitySet::parse(BASELINE.iter().copied()).unwrap(),
        config: &Config::default(),
        probes: &ProbeCache::new(),
    })
    .await
}

fn without(list: &[&str], removed: &[&str]) -> Vec<String> {
    list.iter()
        .filter(|t| !removed.contains(t))
        .map(|&t| t.to_owned())
        .collect()
}

#[test]
fn every_shipped_toolset_expands_to_its_specified_tools() {
    let browser = [
        "browser_exec",
        "browser_cdp",
        "browser_dialog",
        "browser_vault",
    ];
    let file = ["read_file", "write_file", "patch", "search_files"];
    let table: Vec<(&str, Vec<String>)> = vec![
        ("core", strings(CORE)),
        ("web", strings(&["web_search", "web_extract"])),
        ("search", strings(&["web_search"])),
        ("x_search", strings(&["x_search"])),
        ("vision", strings(&["vision_analyze"])),
        ("video", strings(&["video_analyze"])),
        ("image_gen", strings(&["image_generate"])),
        ("video_gen", strings(&["video_generate"])),
        ("computer_use", strings(&["computer_use"])),
        ("terminal", strings(&["terminal", "process"])),
        ("process", strings(&["process"])),
        ("file", strings(&file)),
        (
            "skills",
            strings(&["skills_list", "skill_view", "skill_manage"]),
        ),
        ("browser", strings(&browser)),
        ("cronjob", strings(&["cronjob"])),
        ("tts", strings(&["text_to_speech"])),
        ("todo", strings(&["todo"])),
        ("memory", strings(&["memory"])),
        ("context_engine", Vec::new()),
        ("session_search", strings(&["session_search"])),
        ("connections", strings(&["manage_connections"])),
        ("clarify", strings(&["clarify"])),
        ("code_execution", strings(&["execute_code"])),
        ("delegation", strings(&["delegate_task"])),
        ("kanban", Vec::new()),
        (
            "debugging",
            [
                &["terminal", "process", "web_search", "web_extract"][..],
                &file,
            ]
            .concat()
            .into_iter()
            .map(str::to_owned)
            .collect(),
        ),
        (
            "safe",
            strings(&[
                "web_search",
                "web_extract",
                "vision_analyze",
                "image_generate",
            ]),
        ),
        (
            "coding",
            without(
                CORE,
                &[
                    "image_generate",
                    "text_to_speech",
                    "cronjob",
                    "computer_use",
                ],
            ),
        ),
    ];
    for (name, expected) in &table {
        assert_eq!(&resolve(&[name], &[]).tools, expected, "toolset {name}");
    }
    let mut shipped: Vec<String> = builtin().into_iter().map(|t| t.name).collect();
    let mut listed: Vec<String> = table.iter().map(|(n, _)| (*n).to_owned()).collect();
    shipped.sort();
    listed.sort();
    assert_eq!(shipped, listed, "no toolset is missing from the table");
    assert_eq!(CORE.len(), 27);
}

#[test]
fn default_off_and_posture_flags() {
    let toolsets = builtin();
    let flagged = |pick: fn(&pmt_tools::Toolset) -> bool| -> Vec<String> {
        toolsets
            .iter()
            .filter(|t| pick(t))
            .map(|t| t.name.clone())
            .collect()
    };
    assert_eq!(
        flagged(|t| t.default_off),
        ["x_search", "video", "video_gen", "kanban"]
    );
    assert_eq!(flagged(|t| t.posture), ["coding"]);
    // Default-off toolsets join `all` only when named explicitly.
    let all = resolve(&["all"], &[]);
    for name in ["x_search", "video_analyze", "video_generate"] {
        assert!(!all.contains(name), "{name}");
    }
}

#[tokio::test]
async fn safe_has_no_execution() {
    let surface = surface(&["safe"], &[]).await;
    let names: Vec<&str> = surface.names().collect();
    assert_eq!(
        names,
        [
            "web_search",
            "web_extract",
            "vision_analyze",
            "image_generate"
        ]
    );
    for absent in ["terminal", "process", "write_file", "patch", "execute_code"] {
        assert!(surface.get(absent).is_none(), "{absent}");
    }
    assert!(!names.iter().any(|n| n.starts_with("browser_")));
}

#[tokio::test]
async fn coding_drops_media_and_scheduling() {
    let surface = surface(&["coding"], &[]).await;
    for absent in [
        "image_generate",
        "text_to_speech",
        "cronjob",
        "computer_use",
    ] {
        assert!(surface.get(absent).is_none(), "{absent}");
    }
    for present in ["delegate_task", "browser_exec", "clarify"] {
        assert!(surface.get(present).is_some(), "{present}");
    }
    assert!(surface.posture());
}

#[tokio::test]
async fn default_off_toolset_needs_explicit_naming() {
    let surface = surface(&["core"], &[]).await;
    assert_eq!(surface.excluded()["x_search"].to_string(), "not_in_toolset");
    let surface = self::surface(&["core", "x_search"], &[]).await;
    assert!(surface.get("x_search").is_some());
}

#[tokio::test]
async fn disabling_browser_keeps_search() {
    let surface = surface(&["core"], &["browser"]).await;
    assert!(surface.get("web_search").is_some());
    for name in [
        "browser_exec",
        "browser_cdp",
        "browser_dialog",
        "browser_vault",
    ] {
        assert_eq!(surface.excluded()[name].to_string(), "disabled", "{name}");
    }
    let names: Vec<&str> = surface.names().collect();
    assert_eq!(names.len(), CORE.len() - 4);
}

/// spec: agent-turn-loop, clarify tool contract — surfaces with no
/// interactive channel (cron, API, `-z`, kanban workers) do not get it.
#[test]
fn a_surface_without_an_interactive_channel_drops_clarify() {
    let registry = ToolRegistry::new(
        "default",
        Arc::new(Builtins::standard(pmt_tools::agent_level::tools()).unwrap()),
    );
    let selection = Selection::resolve(&registry, &["core".to_owned()], &[]).unwrap();
    assert!(selection.contains("clarify"));
    let unattended = selection.without_interactive(&registry);
    assert!(!unattended.contains("clarify"));
    assert!(unattended.disabled.contains("clarify"));
    assert!(unattended.contains("todo") && unattended.contains("session_search"));
}
