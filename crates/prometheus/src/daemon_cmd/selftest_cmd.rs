//! `prometheus gateway selftest <platform> [--peer-agent]` (spec:
//! platform-adapters, Peer-agent routing self-test): the selected
//! profile's adapter of `platform` is built (never connected) and
//! synthetic messages go through the gateway's own authorization, group
//! gating and bot admission ([`pmt_gateway::messaging::selftest`]).

use std::fmt::Write as _;

use clap::Args;
use pmt_core::error::Error;
use pmt_gateway::messaging::selftest::{self, Options};

use crate::context::Context;

/// `gateway selftest` arguments.
#[derive(Debug, Clone, Args)]
pub struct SelftestArgs {
    /// The platform (`slack`, `discord`, an adapter plugin's).
    pub platform: String,
    /// Also replay another bot mentioning this agent.
    #[arg(long)]
    pub peer_agent: bool,
    /// The peer bot's sender id (with `--peer-agent`).
    #[arg(long, value_name = "ID", requires = "peer_agent")]
    pub peer_id: Option<String>,
}

/// Runs the self-test; exit 0 when every check passes, else 1.
///
/// # Errors
///
/// The profile cannot be loaded.
pub(super) fn run(args: &SelftestArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let platform = args.platform.to_lowercase();
    let factories = super::messaging::factories(ctx);
    let plugin = crate::plugins_host::adapter::factory();
    let factory = factories.get(&platform).or_else(|| {
        pmt_config::platforms::plugin_platforms()
            .contains(&platform)
            .then_some(&plugin)
    });
    let options = Options {
        peer_agent: args.peer_agent,
        peer_id: args.peer_id.clone(),
    };
    let report = selftest::run(&scope, &platform, factory, &options);
    let code = if report.passed() {
        pmt_core::exit::SUCCESS
    } else {
        pmt_core::exit::FAILURE
    };
    if ctx.json {
        crate::output::json(&report)?;
        return Ok(code);
    }
    let mut text = String::new();
    for check in &report.checks {
        let verdict = match check.category {
            None => "ok".to_owned(),
            Some(category) => format!(
                "FAIL {}",
                serde_json::to_value(category)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default()
            ),
        };
        let _ = writeln!(text, "{verdict:<28} {}: {}", check.name, check.detail);
    }
    crate::output::print_raw(&text)?;
    Ok(code)
}
