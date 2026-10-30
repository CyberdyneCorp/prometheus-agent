use std::sync::Arc;

use pmt_core::capability::CapabilitySet;
use pmt_core::doctor::Status;
use serde_json::json;

use super::{Builtins, ToolRegistry};
use crate::descriptor::{Owner, RegisteredTool, ToolDescriptor};
use crate::testing::{builtin, echo, tool};
use crate::toolset::Toolset;

fn builtins() -> Arc<Builtins> {
    Arc::new(
        Builtins::standard(vec![
            builtin("read_file", "file"),
            builtin("write_file", "file"),
            builtin("web_search", "web"),
        ])
        .unwrap(),
    )
}

fn none() -> CapabilitySet {
    CapabilitySet::new()
}

fn plugin(id: &str) -> Owner {
    Owner::Plugin(id.into())
}

#[test]
fn schema_root_that_is_an_array_is_rejected_and_not_exposed() {
    let mut registry = ToolRegistry::new("default", builtins());
    let descriptor = ToolDescriptor::new("lookup", "file", "Look up.", json!([]));
    let bad = RegisteredTool::new(descriptor, plugin("p"), echo("lookup"));
    let err = registry.register(bad, &none()).unwrap_err();
    assert_eq!(
        err.message(),
        "tool 'lookup': parameters schema root must be an object"
    );
    assert_eq!(err.exit_code(), 78);
    assert!(registry.get("lookup").is_err());
    assert_eq!(registry.generation(), 0);
}

#[test]
fn unknown_toolset_is_named_and_not_created() {
    let mut registry = ToolRegistry::new("default", builtins());
    let err = registry
        .register(tool("grep", "fiel", plugin("p")), &none())
        .unwrap_err();
    assert_eq!(err.code().code, "tool.invalid_registration");
    assert!(
        err.message()
            .starts_with("tool 'grep': unknown toolset 'fiel'"),
        "{}",
        err.message()
    );
    assert!(registry.toolset("fiel").is_none());
}

#[test]
fn second_mcp_server_cannot_take_a_name_and_doctor_reports_it() {
    let mut registry = ToolRegistry::new("default", builtins());
    let owner_a = Owner::Mcp("a".into());
    registry
        .register_toolsets(&owner_a, vec![Toolset::new("mcp-a", "", &[])])
        .unwrap();
    registry
        .register(tool("mcp__a__x", "mcp-a", owner_a.clone()), &none())
        .unwrap();
    let err = registry
        .register(tool("mcp__a__x", "mcp-a", Owner::Mcp("b".into())), &none())
        .unwrap_err();
    assert_eq!(err.code().code, "tool.shadowed");
    assert_eq!(registry.get("mcp__a__x").unwrap().owner, owner_a);
    let checks = registry.doctor_checks();
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].status, Status::Warn);
    assert!(
        checks[0].message.contains("MCP server 'b'"),
        "{}",
        checks[0].message
    );
    // The same server reconnecting replaces its own entry.
    let generation = registry.generation();
    registry
        .register(tool("mcp__a__x", "mcp-a", owner_a), &none())
        .unwrap();
    assert!(registry.generation() > generation);
}

#[test]
fn plugin_override_needs_the_grant() {
    let mut registry = ToolRegistry::new("default", builtins());
    let err = registry
        .register(tool("read_file", "file", plugin("p")), &none())
        .unwrap_err();
    assert_eq!(err.code().code, "tool.shadowed");
    assert_eq!(registry.get("read_file").unwrap().owner, Owner::Builtin);
    assert_eq!(
        err.hint(),
        Some("a plugin replaces a built-in tool only with the grant tools.override:read_file")
    );

    let grant = CapabilitySet::parse(["tools.override:read_file"]).unwrap();
    registry
        .register(tool("read_file", "file", plugin("p")), &grant)
        .unwrap();
    assert_eq!(registry.get("read_file").unwrap().owner, plugin("p"));
    // The grant names one tool; another built-in stays protected.
    assert!(
        registry
            .register(tool("write_file", "file", plugin("p")), &grant)
            .is_err()
    );
    // A grant never lets one plugin take another plugin's tool.
    let any = CapabilitySet::parse(["tools.override:*"]).unwrap();
    assert!(
        registry
            .register(tool("read_file", "file", plugin("q")), &any)
            .is_err()
    );
}

#[test]
fn plugin_tool_does_not_leak_across_profiles() {
    let shared = builtins();
    let mut work = ToolRegistry::new("work", Arc::clone(&shared));
    let home = ToolRegistry::new("home", shared);
    work.register(tool("jira_lookup", "web", plugin("jira")), &none())
        .unwrap();
    assert!(work.get("jira_lookup").is_ok());
    let err = home.get("jira_lookup").unwrap_err();
    assert_eq!(err.message(), "unknown tool: jira_lookup");
    assert_eq!(err.exit_code(), 2);
    assert!(
        !home
            .members("web")
            .unwrap()
            .contains(&"jira_lookup".to_owned())
    );
    assert!(
        work.members("web")
            .unwrap()
            .contains(&"jira_lookup".to_owned())
    );
}

#[test]
fn generation_increases_on_every_mutation() {
    let mut registry = ToolRegistry::new("default", builtins());
    let owner = plugin("p");
    let mut seen = vec![registry.generation()];
    registry
        .register_toolsets(&owner, vec![Toolset::new("acme", "", &[])])
        .unwrap();
    seen.push(registry.generation());
    registry
        .register(tool("acme_one", "acme", owner.clone()), &none())
        .unwrap();
    seen.push(registry.generation());
    registry.unregister(&owner);
    seen.push(registry.generation());
    assert!(seen.windows(2).all(|w| w[0] < w[1]), "{seen:?}");
    assert!(registry.get("acme_one").is_err());
    assert!(registry.toolset("acme").is_none());
}

#[test]
fn toolset_cycle_rejects_the_whole_batch() {
    let mut registry = ToolRegistry::new("default", builtins());
    let owner = plugin("p");
    let err = registry
        .register_toolsets(
            &owner,
            vec![
                Toolset::new("a", "", &[]).including(&["b"]),
                Toolset::new("b", "", &[]).including(&["a"]),
            ],
        )
        .unwrap_err();
    assert_eq!(err.message(), "toolset include cycle: a -> b -> a");
    assert!(registry.toolset("a").is_none() && registry.toolset("b").is_none());
    assert_eq!(registry.rejections().len(), 1);
    assert_eq!(registry.generation(), 0);
}

#[test]
fn toolset_name_of_another_owner_is_refused() {
    let mut registry = ToolRegistry::new("default", builtins());
    let err = registry
        .register_toolsets(&plugin("p"), vec![Toolset::new("web", "", &[])])
        .unwrap_err();
    assert_eq!(err.code().code, "registry.duplicate_name");
}

#[test]
fn builtins_reject_duplicates_and_bad_toolsets() {
    let twice = Builtins::standard(vec![
        builtin("read_file", "file"),
        builtin("read_file", "file"),
    ]);
    assert_eq!(twice.unwrap_err().code().code, "registry.duplicate_name");
    let bad = Builtins::standard(vec![builtin("read_file", "fiel")]);
    assert_eq!(bad.unwrap_err().code().code, "tool.invalid_registration");
}

#[test]
fn members_list_tools_then_registered_then_includes() {
    let mut registry = ToolRegistry::new("default", builtins());
    registry
        .register(tool("web_archive", "web", plugin("p")), &none())
        .unwrap();
    assert_eq!(
        registry.members("safe").unwrap(),
        [
            "web_search",
            "web_extract",
            "web_archive",
            "vision_analyze",
            "image_generate"
        ]
    );
}
