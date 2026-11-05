//! Model-dependent schema variants of the file tools (spec: tool-system,
//! Model-dependent schema variants): `patch` advertises V4A only to
//! OpenAI-family routes, `read_file` mentions scanned PDFs only when
//! hosted OCR is reachable, and the handler accepts every variant's
//! arguments whatever was advertised.

use std::sync::Arc;

use pmt_config::schema::{Config, WireFormat};
use pmt_core::capability::{BASELINE, CapabilitySet};
use pmt_tools::environments::{Environments, SessionEnv};
use pmt_tools::probe::ProbeCache;
use pmt_tools::result::Status;
use pmt_tools::surface::ToolDefinition;
use pmt_tools::variants::{OCR, RouteTraits};
use pmt_tools::{Builtins, Selection, SurfaceInputs, ToolRegistry, ToolSurface};
use serde_json::json;

use crate::dispatch::common;
use crate::support::Fixture;

async fn surface(route: &RouteTraits) -> ToolSurface {
    let tools = pmt_tools::builtin_tools(&Arc::new(Environments::default()));
    let registry = ToolRegistry::new("default", Arc::new(Builtins::standard(tools).unwrap()));
    let selection = Selection::resolve(&registry, &["file".to_owned()], &[]).unwrap();
    ToolSurface::build(SurfaceInputs {
        registry: &registry,
        selection: &selection,
        capabilities: &CapabilitySet::parse(BASELINE.iter().copied()).unwrap(),
        config: &Config::default(),
        probes: &ProbeCache::new(),
        route,
    })
    .await
}

fn definition<'a>(surface: &'a ToolSurface, name: &str) -> &'a ToolDefinition {
    surface
        .definitions()
        .iter()
        .find(|d| d.name == name)
        .unwrap()
}

#[tokio::test]
async fn patch_advertises_v4a_only_to_openai_family_routes() {
    let openai = surface(&RouteTraits::of("openai", "gpt-5", WireFormat::Responses)).await;
    let patch = definition(&openai, "patch");
    assert_eq!(openai.variant("patch"), Some("v4a"));
    assert_eq!(
        patch.parameters["properties"]["mode"]["enum"],
        json!(["replace", "v4a"])
    );
    assert!(patch.parameters["properties"]["patch"].is_object());

    let anthropic = surface(&RouteTraits::of(
        "anthropic",
        "claude-sonnet-4",
        WireFormat::AnthropicMessages,
    ))
    .await;
    let patch = definition(&anthropic, "patch");
    assert_eq!(anthropic.variant("patch"), Some("replace"));
    let properties = patch.parameters["properties"].as_object().unwrap();
    assert!(!properties.contains_key("mode") && !properties.contains_key("patch"));
    assert!(!patch.description.contains("v4a"), "{}", patch.description);
    assert_ne!(openai.serialized(), anthropic.serialized());
}

#[tokio::test]
async fn read_file_mentions_scanned_pdfs_only_when_ocr_is_reachable() {
    let mut route = RouteTraits::of("openai", "gpt-5", WireFormat::Responses);
    let plain = surface(&route).await;
    assert_eq!(plain.variant("read_file"), Some("text"));
    assert!(!definition(&plain, "read_file").description.contains("OCR"));
    route.auxiliary.insert(OCR.to_owned());
    let ocr = surface(&route).await;
    assert_eq!(ocr.variant("read_file"), Some("scanned_pdf"));
    assert!(
        definition(&ocr, "read_file")
            .description
            .contains("hosted OCR")
    );
}

/// Scenario: Handler accepts any variant — a V4A-shaped call (from a
/// resumed history) runs although the surface advertises replace only.
#[tokio::test]
async fn handler_accepts_any_variant() {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {\n    1\n}\n");
    let environments = Arc::new(Environments::default());
    let _attached = environments.attach(
        "ses_variants",
        SessionEnv {
            files: Some(Arc::new(fx.session())),
            ..SessionEnv::default()
        },
    );
    // The test dispatcher's route is unknown: the replace-only variant.
    assert_eq!(
        surface(&RouteTraits::default()).await.variant("patch"),
        Some("replace")
    );
    let dispatcher = common::dispatcher(
        &environments,
        "ses_variants",
        &Config::default(),
        &[],
        &fx.outside,
    )
    .await;
    let patch =
        "*** Begin Patch\n*** Update File: a.rs\n@@\n fn a() {\n-    1\n+    2\n }\n*** End Patch";
    let result = common::call(&dispatcher, "patch", json!({"mode": "v4a", "patch": patch})).await;
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    assert_eq!(fx.read("a.rs"), "fn a() {\n    2\n}\n");
}
