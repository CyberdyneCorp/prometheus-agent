use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use pmt_config::schema::Config;
use pmt_core::capability::{BASELINE, Capability, CapabilitySet};

use super::{Exclusion, SurfaceInputs, ToolSurface};
use crate::descriptor::{Owner, RegisteredTool};
use crate::probe::{Probe, ProbeCache, ProbeContext, ProbeFuture};
use crate::registry::{Builtins, ToolRegistry};
use crate::selection::Selection;
use crate::testing::{builtin, tool};
use crate::toolset::CORE;

/// A probe the test switches on and off, counting runs.
struct Switch {
    id: &'static str,
    up: AtomicBool,
    runs: AtomicUsize,
}

impl Switch {
    fn new(id: &'static str, up: bool) -> Arc<Self> {
        Arc::new(Self {
            id,
            up: AtomicBool::new(up),
            runs: AtomicUsize::new(0),
        })
    }
}

impl Probe for Switch {
    fn id(&self) -> &str {
        self.id
    }
    fn check<'a>(&'a self, _: ProbeContext<'a>) -> ProbeFuture<'a> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let up = self.up.load(Ordering::SeqCst);
        Box::pin(async move {
            if up {
                Ok(())
            } else {
                Err(format!("{} not found", self.id))
            }
        })
    }
}

fn web_extract() -> RegisteredTool {
    let mut web = builtin("web_extract", "web");
    web.descriptor.capabilities = vec![Capability::parse("net.connect:*").unwrap()];
    web
}

/// Every `core` tool registered as a built-in; browser tools probe for a
/// runtime and `terminal` for the container daemon.
fn core_builtins(browser: &Arc<Switch>, container: &Arc<Switch>) -> Arc<Builtins> {
    let tools = CORE
        .iter()
        .map(|&name| {
            let toolset = if name.starts_with("browser_") {
                "browser"
            } else {
                "core"
            };
            let mut entry = if name == "web_extract" {
                web_extract()
            } else {
                builtin(name, toolset)
            };
            if name.starts_with("browser_") {
                entry = entry.with_probe(Arc::clone(browser) as Arc<dyn Probe>);
            }
            if name == "terminal" {
                entry = entry.with_probe(Arc::clone(container) as Arc<dyn Probe>);
            }
            entry
        })
        .chain([builtin("x_search", "x_search")])
        .collect();
    Arc::new(Builtins::standard(tools).unwrap())
}

struct Fixture {
    registry: ToolRegistry,
    browser: Arc<Switch>,
    container: Arc<Switch>,
    probes: ProbeCache,
    config: Config,
}

impl Fixture {
    fn new() -> Self {
        let (browser, container) = (Switch::new("browser", true), Switch::new("container", true));
        let registry = ToolRegistry::new("default", core_builtins(&browser, &container));
        Self {
            registry,
            browser,
            container,
            probes: ProbeCache::new(),
            config: Config::default(),
        }
    }

    async fn build(
        &self,
        enabled: &[&str],
        disabled: &[&str],
        caps: &CapabilitySet,
    ) -> ToolSurface {
        let list = |names: &[&str]| names.iter().map(|&n| n.to_owned()).collect::<Vec<_>>();
        let selection =
            Selection::resolve(&self.registry, &list(enabled), &list(disabled)).unwrap();
        ToolSurface::build(SurfaceInputs {
            registry: &self.registry,
            selection: &selection,
            capabilities: caps,
            config: &self.config,
            probes: &self.probes,
        })
        .await
    }
}

fn baseline() -> CapabilitySet {
    CapabilitySet::parse(BASELINE.iter().copied()).unwrap()
}

fn reason(surface: &ToolSurface, tool: &str) -> String {
    surface.excluded()[tool].to_string()
}

#[tokio::test]
async fn missing_capability_hides_a_tool() {
    let fixture = Fixture::new();
    let no_net = baseline().without(&Capability::parse("net.connect:*").unwrap());
    let surface = fixture.build(&["core"], &[], &no_net).await;
    assert!(surface.get("web_extract").is_none());
    assert_eq!(
        reason(&surface, "web_extract"),
        "missing_capability:net.connect:*"
    );
    assert!(surface.get("web_search").is_some());
    let surface = fixture.build(&["core"], &[], &baseline()).await;
    assert!(surface.get("web_extract").is_some());
}

#[tokio::test]
async fn browser_unavailable_keeps_the_rest_of_core() {
    let fixture = Fixture::new();
    fixture.browser.up.store(false, Ordering::SeqCst);
    let surface = fixture.build(&["core"], &[], &baseline()).await;
    for name in [
        "browser_exec",
        "browser_cdp",
        "browser_dialog",
        "browser_vault",
    ] {
        assert_eq!(reason(&surface, name), "unavailable:browser not found");
    }
    let names: Vec<&str> = surface.names().collect();
    let expected: Vec<&str> = CORE
        .iter()
        .copied()
        .filter(|n| !n.starts_with("browser_"))
        .collect();
    assert_eq!(names, expected, "every other core tool, in core order");
    assert_eq!(
        fixture.browser.runs.load(Ordering::SeqCst),
        1,
        "one probe run per build"
    );
}

#[tokio::test]
async fn exclusions_carry_exactly_one_reason() {
    let fixture = Fixture::new();
    let surface = fixture.build(&["core"], &["browser"], &baseline()).await;
    assert_eq!(surface.excluded()["x_search"], Exclusion::NotInToolset);
    assert_eq!(surface.excluded()["browser_exec"], Exclusion::Disabled);
    assert!(surface.get("web_search").is_some());
    assert_eq!(
        fixture.browser.runs.load(Ordering::SeqCst),
        0,
        "excluded tools are not probed"
    );
    let surface = fixture.build(&["core", "x_search"], &[], &baseline()).await;
    assert!(surface.get("x_search").is_some());
}

#[tokio::test]
async fn identical_inputs_serialize_byte_identically() {
    let fixture = Fixture::new();
    let a = fixture.build(&["core"], &[], &baseline()).await;
    let b = fixture.build(&["core"], &[], &baseline()).await;
    assert_eq!(a.generation(), b.generation());
    assert_eq!(a.serialized(), b.serialized());
    assert!(a.serialized().starts_with(
        r#"[{"name":"web_search","description":"The web_search tool.","parameters":{"#
    ));
}

#[tokio::test]
async fn changing_toolsets_mid_session_affects_only_the_next_session() {
    let mut fixture = Fixture::new();
    let session = fixture.build(&["file"], &[], &baseline()).await;
    let frozen = session.serialized().to_owned();

    // Mid-session: config now selects `web`, and a plugin adds a tool.
    fixture
        .registry
        .register(
            tool("web_archive", "web", Owner::Plugin("p".into())),
            &CapabilitySet::new(),
        )
        .unwrap();
    assert_eq!(
        session.serialized(),
        frozen,
        "the running session keeps its array"
    );
    assert!(session.get("web_search").is_none());
    assert!(session.get("read_file").is_some());

    let next = fixture.build(&["web"], &[], &baseline()).await;
    assert!(next.generation() > session.generation());
    assert_eq!(
        next.names().collect::<Vec<_>>(),
        ["web_search", "web_extract", "web_archive"]
    );
    assert_ne!(next.serialized(), frozen);
}

#[tokio::test(start_paused = true)]
async fn credential_added_mid_session_waits_for_the_next_session() {
    let fixture = Fixture::new();
    fixture.browser.up.store(false, Ordering::SeqCst);
    let session = fixture.build(&["core"], &[], &baseline()).await;
    assert!(session.get("browser_exec").is_none());

    fixture.browser.up.store(true, Ordering::SeqCst);
    assert!(session.get("browser_exec").is_none(), "frozen");
    tokio::time::advance(Duration::from_secs(31)).await;
    let next = fixture.build(&["core"], &[], &baseline()).await;
    assert!(next.get("browser_exec").is_some());
}

#[tokio::test(start_paused = true)]
async fn transient_container_failure_keeps_terminal() {
    let fixture = Fixture::new();
    fixture.build(&["core"], &[], &baseline()).await;
    tokio::time::advance(Duration::from_secs(40)).await;
    fixture.container.up.store(false, Ordering::SeqCst);
    let surface = fixture.build(&["core"], &[], &baseline()).await;
    assert!(surface.get("terminal").is_some());
    let runs = fixture.container.runs.load(Ordering::SeqCst);
    fixture.build(&["core"], &[], &baseline()).await;
    assert_eq!(
        fixture.container.runs.load(Ordering::SeqCst),
        runs + 1,
        "re-probed"
    );

    tokio::time::advance(Duration::from_secs(61)).await;
    let surface = fixture.build(&["core"], &[], &baseline()).await;
    assert_eq!(
        reason(&surface, "terminal"),
        "unavailable:container not found"
    );
}

#[tokio::test]
async fn descriptions_reference_only_present_tools() {
    let rewriter: crate::descriptor::Rewriter = Arc::new(|base, exposed| {
        if exposed.contains("vision_analyze") {
            format!("{base} Use vision_analyze on the screenshot for visual details.")
        } else {
            base.to_owned()
        }
    });
    let snapshot = builtin("browser_snapshot", "browser").with_rewriter(rewriter);
    let builtins = Builtins::standard(vec![snapshot, builtin("vision_analyze", "vision")]).unwrap();
    let registry = ToolRegistry::new("default", Arc::new(builtins));
    let (config, probes) = (Config::default(), ProbeCache::new());
    let build = |enabled: Vec<String>| {
        let selection = Selection::resolve(&registry, &enabled, &[]).unwrap();
        let (registry, config, probes) = (&registry, &config, &probes);
        async move {
            ToolSurface::build(SurfaceInputs {
                registry,
                selection: &selection,
                capabilities: &CapabilitySet::new(),
                config,
                probes,
            })
            .await
        }
    };
    let without = build(vec!["browser".into()]).await;
    assert!(
        !without.definitions()[0]
            .description
            .contains("vision_analyze")
    );
    let with = build(vec!["browser".into(), "vision".into()]).await;
    assert!(with.definitions()[0].description.contains("vision_analyze"));
}
