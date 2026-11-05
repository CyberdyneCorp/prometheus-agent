//! Surface resolution (spec: tool-system, Per-surface default toolsets;
//! Platform bundles; Disabling a bundle never strips core): the toolsets
//! each session surface starts from, the platform bundles and what
//! disabling a toolset removes.

use std::sync::Arc;

use pmt_config::schema::{Config, SurfaceToolsets, ToolsConfig, ToolsetsConfig};
use pmt_core::capability::{BASELINE, CapabilitySet};
use pmt_tools::descriptor::{CallContext, HandlerFuture, Owner, ToolHandler};
use pmt_tools::probe::ProbeCache;
use pmt_tools::toolset::{CORE, builtin, builtin_tool_names};
use pmt_tools::{
    Builtins, RegisteredTool, Selection, SessionSurface, SurfaceInputs, ToolDescriptor,
    ToolRegistry, ToolResult, ToolSurface, Toolset,
};
use serde_json::{Value, json};

struct Ok;

impl ToolHandler for Ok {
    fn call<'a>(&'a self, _: &'a CallContext, _: Value) -> HandlerFuture<'a> {
        Box::pin(async { Result::Ok(ToolResult::ok("ok")) })
    }
}

fn tool(name: &str, toolset: &str, owner: Owner) -> RegisteredTool {
    let descriptor = ToolDescriptor::new(name, toolset, "A tool.", json!({"type": "object"}));
    RegisteredTool::new(descriptor, owner, Arc::new(Ok))
}

/// Every shipped tool, registered under the first toolset listing it.
fn registry() -> ToolRegistry {
    let toolsets = builtin();
    let tools = builtin_tool_names()
        .into_iter()
        .map(|name| {
            let home = toolsets
                .iter()
                .find(|t| t.name != "core" && t.tools.contains(&name))
                .map_or("core", |t| t.name.as_str());
            tool(&name, home, Owner::Builtin)
        })
        .collect();
    ToolRegistry::new("default", Arc::new(Builtins::standard(tools).unwrap()))
}

/// `registry` plus `toolsets` (each holding the tools listed with it)
/// registered by `owner`.
fn with_toolsets(
    mut registry: ToolRegistry,
    owner: &Owner,
    toolsets: &[(&str, &[&str])],
) -> ToolRegistry {
    let batch = toolsets
        .iter()
        .map(|(name, _)| Toolset::new(name, "", &[]))
        .collect();
    registry.register_toolsets(owner, batch).unwrap();
    for (name, tools) in toolsets {
        for t in *tools {
            registry
                .register(tool(t, name, owner.clone()), &CapabilitySet::default())
                .unwrap();
        }
    }
    registry
}

fn strings(names: &[&str]) -> Vec<String> {
    names.iter().map(|&n| n.to_owned()).collect()
}

fn surface(key: &str) -> SessionSurface {
    SessionSurface::parse(key).unwrap()
}

fn select(registry: &ToolRegistry, key: &str, config: &Config) -> Selection {
    Selection::for_surface(registry, &surface(key), &config.tools, &config.toolsets).unwrap()
}

async fn exposed(registry: &ToolRegistry, selection: &Selection) -> Vec<String> {
    ToolSurface::build(SurfaceInputs {
        registry,
        selection,
        capabilities: &CapabilitySet::parse(BASELINE.iter().copied()).unwrap(),
        config: &Config::default(),
        probes: &ProbeCache::new(),
        route: &pmt_tools::variants::RouteTraits::default(),
    })
    .await
    .names()
    .map(str::to_owned)
    .collect()
}

fn disabling(names: &[&str]) -> Config {
    Config {
        tools: ToolsConfig {
            disabled_toolsets: strings(names),
            ..ToolsConfig::default()
        },
        ..Config::default()
    }
}

/// Built-in defaults: `core` for cli, tui, desktop and cron; `api_server`
/// for the API; the platform bundle for a messaging platform; `core` for
/// a platform without one (the relay).
#[test]
fn each_surface_starts_from_its_builtin_default() {
    let registry = registry();
    let config = Config::default();
    let starting =
        |key: &str| surface(key).starting_toolsets(&registry, &config.tools, &config.toolsets);
    for key in ["cli", "tui", "desktop", "cron"] {
        assert_eq!(starting(key), ["core"], "{key}");
    }
    assert_eq!(starting("api"), ["api_server"]);
    assert_eq!(starting("acp"), ["acp"]);
    assert_eq!(starting("acp_unattended"), ["acp_unattended"]);
    assert_eq!(starting("gateway.telegram"), ["platform-telegram"]);
    assert_eq!(starting("gateway.webhook"), ["webhook-safe"]);
    assert_eq!(starting("gateway.relay"), ["core"]);
    let api = select(&registry, "api", &config);
    for absent in ["text_to_speech", "clarify", "computer_use"] {
        assert!(!api.contains(absent), "{absent}");
    }
    assert!(api.contains("terminal") && api.contains("web_search"));
    assert!(SessionSurface::parse("desk").is_none());
    assert!(SessionSurface::parse("gateway.").is_none());
}

/// `core` in `tools.enabled_toolsets` stands for the surface's default;
/// the other names are kept.
#[test]
fn the_profile_list_applies_with_core_as_the_surface_default() {
    let registry = registry();
    let config = Config {
        tools: ToolsConfig {
            enabled_toolsets: strings(&["core", "x_search"]),
            ..ToolsConfig::default()
        },
        ..Config::default()
    };
    let starting =
        |key: &str| surface(key).starting_toolsets(&registry, &config.tools, &config.toolsets);
    assert_eq!(starting("cli"), ["core", "x_search"]);
    assert_eq!(
        starting("gateway.discord"),
        ["platform-discord", "x_search"]
    );
    assert_eq!(starting("api"), ["api_server", "x_search"]);
}

/// Scenario: Cron override.
#[test]
fn cron_override() {
    let registry = registry();
    let mut config = Config {
        toolsets: ToolsetsConfig {
            surfaces: SurfaceToolsets {
                cron: Some(strings(&["web", "file"])),
                ..SurfaceToolsets::default()
            },
        },
        ..Config::default()
    };
    let cron = select(&registry, "cron", &config);
    assert_eq!(
        cron.tools,
        strings(&[
            "web_search",
            "web_extract",
            "read_file",
            "write_file",
            "patch",
            "search_files"
        ])
    );
    // Other surfaces keep their own default.
    assert!(select(&registry, "cli", &config).contains("terminal"));
    // Minus the disabled toolsets.
    config.tools.disabled_toolsets = strings(&["web"]);
    let cron = select(&registry, "cron", &config);
    assert_eq!(
        cron.tools,
        strings(&["read_file", "write_file", "patch", "search_files"])
    );
}

/// Scenario: Webhook payload cannot execute — also when the profile-wide
/// list enables execution.
#[tokio::test]
async fn webhook_payload_cannot_execute() {
    let registry = registry();
    let expected = strings(&["web_search", "web_extract", "vision_analyze", "clarify"]);
    let selection = select(&registry, "gateway.webhook", &Config::default());
    assert_eq!(exposed(&registry, &selection).await, expected);
    let config = Config {
        tools: ToolsConfig {
            enabled_toolsets: strings(&["core", "terminal", "code_execution"]),
            ..ToolsConfig::default()
        },
        ..Config::default()
    };
    let selection = select(&registry, "gateway.webhook", &config);
    assert_eq!(exposed(&registry, &selection).await, expected);
}

/// Scenario: Feishu tools stay on Feishu.
#[test]
fn feishu_tools_stay_on_feishu() {
    let registry = with_toolsets(
        registry().with_platforms(["feishu"]),
        &Owner::Plugin("feishu".into()),
        &[
            ("feishu_doc", &["feishu_doc_read"]),
            ("feishu_drive", &["feishu_drive_list_comments"]),
        ],
    );
    let config = Config::default();
    let feishu = select(&registry, "gateway.feishu", &config);
    let telegram = select(&registry, "gateway.telegram", &config);
    for name in ["feishu_doc_read", "feishu_drive_list_comments"] {
        assert!(feishu.contains(name), "{name}");
        assert!(!telegram.contains(name), "{name}");
    }
    assert!(feishu.contains("terminal") && telegram.contains("terminal"));
    // `all` never folds platform bundles in.
    let all = Selection::resolve(&registry, &strings(&["all"]), &[]).unwrap();
    assert!(!all.toolsets.iter().any(|t| t.starts_with("platform-")));
}

/// Scenario: Plugin platform bundle.
#[test]
fn plugin_platform_bundle() {
    let registry = with_toolsets(
        registry().with_platforms(["acme"]),
        &Owner::Plugin("acme".into()),
        &[("acme", &["acme_lookup"])],
    );
    let config = Config::default();
    let acme = select(&registry, "gateway.acme", &config);
    let mut expected = strings(CORE);
    expected.push("acme_lookup".to_owned());
    assert_eq!(acme.tools, expected);
    assert!(!select(&registry, "gateway.telegram", &config).contains("acme_lookup"));
    // The union of every bundle holds every platform's tools.
    let gateway = Selection::resolve(&registry, &strings(&["gateway"]), &[]).unwrap();
    assert!(gateway.contains("acme_lookup") && gateway.contains("terminal"));
}

/// Scenario: Disable the Discord bundle; `discord_admin` is never part of
/// the bundle.
#[test]
fn disable_the_discord_bundle() {
    let owner = Owner::Plugin("discord".into());
    let registry = with_toolsets(
        registry(),
        &owner,
        &[
            ("discord", &["discord"]),
            ("discord_admin", &["discord_admin"]),
        ],
    );
    let discord = select(&registry, "gateway.discord", &Config::default());
    assert!(discord.contains("discord") && !discord.contains("discord_admin"));
    let discord = select(
        &registry,
        "gateway.discord",
        &disabling(&["platform-discord"]),
    );
    assert!(!discord.contains("discord"));
    assert!(discord.contains("terminal") && discord.contains("web_search"));
    // The union and api_server are built on core too.
    let gateway = select(&registry, "gateway.discord", &disabling(&["gateway"]));
    assert!(!gateway.contains("discord") && gateway.contains("terminal"));
    let api = select(&registry, "api", &disabling(&["api_server"]));
    assert!(api.contains("terminal"));
}

/// Disabling `core` itself or a single-purpose toolset removes all of
/// its members; a toolset including `core` keeps it.
#[test]
fn disabling_core_or_a_single_purpose_toolset_removes_everything_it_lists() {
    let registry = registry();
    let gone = select(&registry, "gateway.discord", &disabling(&["core"]));
    assert!(gone.tools.is_empty(), "{:?}", gone.tools);
    let no_web = select(&registry, "gateway.discord", &disabling(&["web"]));
    assert!(!no_web.contains("web_search") && no_web.contains("terminal"));
    let mut registry = registry;
    registry
        .register_toolsets(
            &Owner::Plugin("p".into()),
            vec![Toolset::new("mine", "", &["x_search"]).including(&["core"])],
        )
        .unwrap();
    let config = Config {
        tools: ToolsConfig {
            enabled_toolsets: strings(&["mine"]),
            disabled_toolsets: strings(&["mine"]),
            ..ToolsConfig::default()
        },
        ..Config::default()
    };
    let cli = select(&registry, "cli", &config);
    assert!(!cli.contains("x_search") && cli.contains("terminal"));
}
