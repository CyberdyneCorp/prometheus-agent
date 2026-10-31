//! A dispatcher over the built-in tools, as a session builds it: the
//! shipped toolsets, `core` selected, the baseline capabilities plus
//! `extra`, no hooks and no approval prompts.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pmt_config::schema::{Config, ResultBudget as BudgetConfig};
use pmt_core::cancel::CancelToken;
use pmt_core::capability::{BASELINE, CapabilitySet};
use pmt_core::panic::CrashReporter;
use pmt_core::secret::Redactor;
use pmt_tools::budget::ResultBudget;
use pmt_tools::dispatch::{AllowAll, NoHooks, NoObserver};
use pmt_tools::environments::Environments;
use pmt_tools::probe::ProbeCache;
use pmt_tools::spillover::session_dir;
use pmt_tools::{
    Builtins, DispatchSetup, Dispatcher, Selection, SurfaceInputs, ToolCall, ToolRegistry,
    ToolResult, ToolSurface,
};

/// The dispatcher of session `session_id`, whose tools find their state
/// in `environments`.
pub(crate) async fn dispatcher(
    environments: &Arc<Environments>,
    session_id: &str,
    config: &Config,
    extra: &[&str],
    profile_dir: &Path,
) -> Dispatcher {
    let tools = pmt_tools::builtin_tools(environments);
    let registry = ToolRegistry::new("default", Arc::new(Builtins::standard(tools).unwrap()));
    let selection = Selection::resolve(&registry, &["core".to_owned()], &[]).unwrap();
    let capabilities =
        CapabilitySet::parse(BASELINE.iter().copied().chain(extra.iter().copied())).unwrap();
    let surface = ToolSurface::build(SurfaceInputs {
        registry: &registry,
        selection: &selection,
        capabilities: &capabilities,
        config,
        probes: &ProbeCache::new(),
    })
    .await;
    Dispatcher::new(DispatchSetup {
        session_id: session_id.to_owned(),
        surface: Arc::new(surface),
        capabilities: Arc::new(capabilities),
        hooks: Arc::new(NoHooks),
        gate: Arc::new(AllowAll),
        observer: Arc::new(NoObserver),
        budget: ResultBudget::new(
            BudgetConfig::default(),
            None,
            session_dir(profile_dir, session_id),
            Redactor::default(),
        ),
        reporter: Arc::new(CrashReporter::new(
            profile_dir.join("crash"),
            Redactor::default(),
        )),
        default_timeout: Duration::from_mins(5),
        concurrency: 8,
    })
}

/// Dispatches one call.
pub(crate) async fn call(
    dispatcher: &Dispatcher,
    tool: &str,
    arguments: serde_json::Value,
) -> ToolResult {
    dispatcher
        .dispatch(
            ToolCall::new("call_1", tool, &arguments.to_string()),
            &CancelToken::new(),
        )
        .await
}
