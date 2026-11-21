//! Platform-contributed toolsets (spec: platform-adapters,
//! Platform-contributed toolsets): an adapter's tools exist only in the
//! registries of profiles that enable its platform, inside the platform's
//! bundle, with actions the bot cannot perform hidden.

use std::sync::Arc;

use pmt_config::schema::{GatewayConfig, PlatformConfig};
use pmt_tools::descriptor::{CallContext, HandlerFuture, Owner, ToolHandler};
use pmt_tools::platform_tools::{self, Action, Context, Contributor};
use pmt_tools::{Builtins, RegisteredTool, ToolDescriptor, ToolRegistry, ToolResult};
use serde_json::{Value, json};

struct Answer;

impl ToolHandler for Answer {
    fn call<'a>(&'a self, _: &'a CallContext, _: Value) -> HandlerFuture<'a> {
        Box::pin(async { Ok(ToolResult::ok("ok")) })
    }
}

const ACTIONS: &[Action] = &[
    Action {
        name: "read",
        permission: None,
    },
    Action {
        name: "comment",
        permission: Some("drive:write"),
    },
];

/// A Feishu-like contributor: `feishu_doc_read`, and `feishu_drive` whose
/// action enum follows the detected permissions.
struct Feishu;

impl Contributor for Feishu {
    fn platform(&self) -> &'static str {
        "feishu"
    }

    fn toolsets(&self) -> Vec<(String, String)> {
        vec![
            ("feishu_doc".into(), "Feishu documents".into()),
            ("feishu_drive".into(), "Feishu drive".into()),
        ]
    }

    fn tools(&self, context: &Context<'_>) -> Vec<RegisteredTool> {
        let actions: Vec<&str> = platform_tools::visible(ACTIONS, None, context.granted.as_ref())
            .iter()
            .map(|a| a.name)
            .collect();
        let doc = ToolDescriptor::new(
            "feishu_doc_read",
            "feishu_doc",
            "Read a doc.",
            json!({"type": "object"}),
        );
        let drive = ToolDescriptor::new(
            "feishu_drive",
            "feishu_drive",
            "Drive actions.",
            json!({"type": "object", "properties": {"action": {"enum": actions}}}),
        );
        [doc, drive]
            .into_iter()
            .map(|d| {
                RegisteredTool::new(
                    d,
                    Owner::Capability("platform:feishu".into()),
                    Arc::new(Answer),
                )
            })
            .collect()
    }
}

fn registry(gateway: &GatewayConfig, dir: &std::path::Path) -> ToolRegistry {
    platform_tools::contribute(Arc::new(Feishu));
    let builtins = Builtins::standard(Vec::new()).unwrap();
    let mut registry = ToolRegistry::new("default", Arc::new(builtins)).with_platforms(["feishu"]);
    platform_tools::register(&mut registry, dir, gateway);
    registry
}

fn names(registry: &ToolRegistry) -> Vec<String> {
    registry.tools().map(|t| t.name().to_owned()).collect()
}

/// "Toolset absent elsewhere".
#[test]
fn a_profile_without_the_platform_has_none_of_its_tools() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let registry = registry(&GatewayConfig::default(), dir.path());
    assert!(
        !names(&registry).iter().any(|n| n.starts_with("feishu_")),
        "{:?}",
        names(&registry)
    );
}

#[test]
fn an_enabled_platform_brings_its_tools_into_its_bundle() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mut gateway = GatewayConfig::default();
    gateway.platforms.insert(
        "feishu".into(),
        PlatformConfig {
            enabled: true,
            ..PlatformConfig::default()
        },
    );
    let registry = registry(&gateway, dir.path());
    let bundle = registry.members("platform-feishu").unwrap();
    assert!(bundle.contains(&"feishu_doc_read".to_owned()), "{bundle:?}");
    let schema = |r: &ToolRegistry| r.get("feishu_drive").unwrap().descriptor.parameters.clone();
    assert_eq!(
        schema(&registry)["properties"]["action"]["enum"],
        json!(["read", "comment"])
    );

    let granted = ["drive:read".to_owned()].into();
    platform_tools::store_permissions(dir.path(), "feishu", Some(&granted)).unwrap();
    let narrowed = self::registry(&gateway, dir.path());
    assert_eq!(
        schema(&narrowed)["properties"]["action"]["enum"],
        json!(["read"])
    );
}
