//! `prometheus status [--json] [--check] [--full [--deep]] [--liveness]
//! [--exports]` (spec: observability, Status command; Configuration-
//! centric status; Export posture status; Runtime liveness watchdog and
//! task dump): whether the daemon runs and how it is, read from its
//! control channel and runtime record plus the active profile's own
//! store. Only `--deep` makes outbound requests.

mod daemon;
mod deep;
mod exports;
mod local;

use std::fmt::Write as _;

use pmt_core::error::Error;
use pmt_rpc::control::ComponentReport;
use serde::Serialize;

use crate::context::Context;

/// `prometheus status` arguments.
#[derive(Debug, Clone, clap::Args)]
#[allow(clippy::struct_excessive_bools)] // independent command-line flags
pub struct StatusArgs {
    /// Exit 1 when the daemon is not running or a component is down.
    #[arg(long)]
    pub check: bool,
    /// Add the profile's model, providers with credentials, adapters,
    /// scheduled jobs, terminal backend, sandbox and session counts.
    #[arg(long, conflicts_with_all = ["liveness", "exports"])]
    pub full: bool,
    /// With `--full`: probe each provider, adapter and MCP server live
    /// (10 s each); the only mode that makes outbound requests.
    #[arg(long, requires = "full")]
    pub deep: bool,
    /// Ask the daemon's `/healthz`: exit 0 on 200 (printing nothing), 1
    /// otherwise.
    #[arg(long, conflicts_with_all = ["check", "exports"])]
    pub liveness: bool,
    /// Report every telemetry exporter and what can leave this host.
    #[arg(long)]
    pub exports: bool,
}

#[derive(Serialize)]
struct Report<'a> {
    daemon: &'a daemon::DaemonFacts,
    #[serde(flatten)]
    basics: &'a local::Basics,
    #[serde(skip_serializing_if = "Option::is_none")]
    full: Option<&'a local::Full>,
    #[serde(skip_serializing_if = "Option::is_none")]
    probes: Option<&'a [deep::Probe]>,
}

/// Runs `prometheus status`.
///
/// # Errors
///
/// Home, profile and store errors.
pub fn run(args: &StatusArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let home = ctx.home()?.root().to_path_buf();
    let facts = daemon::facts(&home)?;
    if args.liveness {
        let alive = facts.health_url.as_deref().is_some_and(daemon::healthz);
        if !alive {
            eprintln!("daemon: not live");
        }
        return Ok(u8::from(!alive));
    }
    let scope = ctx.scope()?;
    if args.exports {
        let live = facts.status.as_ref().and_then(|s| s.export.as_ref());
        let posture = exports::posture(&scope, live, facts.health_url.as_deref());
        return if ctx.json {
            crate::output::document("status exports", &posture)
        } else {
            crate::output::print_raw(&exports::render(&posture))
        };
    }
    let basics = local::basics(&scope)?;
    let full = args.full.then(|| local::full(ctx, &scope)).transpose()?;
    let probes = args.deep.then(|| deep::probes(ctx, &scope));
    let failing = failing(&facts);
    if ctx.json {
        crate::output::document(
            "status",
            &Report {
                daemon: &facts,
                basics: &basics,
                full: full.as_ref(),
                probes: probes.as_deref(),
            },
        )?;
    } else {
        let mut text = daemon_text(&facts);
        text.push_str(&basics_text(&basics));
        if let Some(full) = &full {
            text.push_str(&full_text(full));
        }
        if let Some(probes) = &probes {
            text.push_str(&probes_text(probes));
        }
        crate::output::print_raw(&text)?;
    }
    Ok(u8::from(
        args.check && (!facts.running || !failing.is_empty()),
    ))
}

/// The components that are `down`.
fn failing(facts: &daemon::DaemonFacts) -> Vec<&ComponentReport> {
    facts
        .status
        .iter()
        .flat_map(|s| &s.health)
        .filter(|c| c.state == "down")
        .collect()
}

fn daemon_text(facts: &daemon::DaemonFacts) -> String {
    let mut text = String::new();
    let Some(status) = facts.status.as_ref().filter(|_| facts.running) else {
        text.push_str(if facts.running {
            "daemon: running (not answering)\n"
        } else {
            "daemon: not running\n"
        });
        if let Some(last) = &facts.last_exit {
            let _ = writeln!(
                text,
                "last daemon: {} ({}) at {}",
                last["state"].as_str().unwrap_or("?"),
                last["exit_reason"].as_str().unwrap_or("?"),
                last["updated_at"].as_str().unwrap_or("?")
            );
        }
        return text;
    };
    let build = if facts.build_match {
        "build matches"
    } else {
        "older build: run 'prometheus daemon restart'"
    };
    let _ = writeln!(
        text,
        "daemon: running (pid {}, up {}s, version {}, {build})",
        status.pid, status.uptime_secs, status.version
    );
    if let Some(stale) = facts.heartbeat_stale_secs {
        let _ = writeln!(text, "warning: daemon heartbeat stale ({stale}s)");
    }
    if facts.state.as_deref() == Some("degraded") {
        text.push_str("warning: daemon state is degraded\n");
    }
    let _ = writeln!(text, "profiles: {}", status.profiles.join(", "));
    let _ = writeln!(
        text,
        "sessions: {} active, {} running turns, {} subagents",
        status.sessions, status.active_turns, status.subagents
    );
    text.push_str("components:\n");
    if status.health.is_empty() {
        text.push_str("  (none reported)\n");
    }
    for c in &status.health {
        let owner = c
            .profile
            .as_deref()
            .map(|p| format!(" ({p})"))
            .unwrap_or_default();
        let reason = if c.reason.is_empty() {
            String::new()
        } else {
            format!(": {}", c.reason)
        };
        let _ = writeln!(text, "  {:<9}{}{owner}{reason}", c.state, c.component);
    }
    text
}

fn basics_text(basics: &local::Basics) -> String {
    let mut text = String::new();
    match &basics.next_job {
        Some(job) => {
            let _ = writeln!(text, "next job: {} at {}", job.name, job.at);
        }
        None => text.push_str("next job: none scheduled\n"),
    }
    let spend = &basics.spend_today;
    let limit = spend.daily_usd.map_or_else(
        || "no daily budget".to_owned(),
        |l| format!("of ${l:.2} (budgets.daily_usd)"),
    );
    let _ = writeln!(
        text,
        "spend today: ${:.4} {limit}, {} unpriced calls",
        spend.usd, spend.unpriced_calls
    );
    text
}

fn full_text(full: &local::Full) -> String {
    let mut text = format!("model: {} / provider {}\n", full.model, full.provider);
    let creds = if full.providers_with_credentials.is_empty() {
        "(none)".to_owned()
    } else {
        full.providers_with_credentials.join(", ")
    };
    let _ = writeln!(text, "providers with credentials: {creds}");
    for (profile, adapters) in &full.adapters {
        let list = if adapters.is_empty() {
            "(none)".to_owned()
        } else {
            adapters.join(", ")
        };
        let _ = writeln!(text, "adapters ({profile}): {list}");
    }
    for job in &full.jobs {
        let _ = writeln!(text, "job: {} next at {}", job.name, job.at);
    }
    let _ = writeln!(
        text,
        "terminal: backend {}, sandbox {}",
        full.terminal_backend, full.sandbox
    );
    let s = full.sessions;
    let _ = writeln!(
        text,
        "sessions: {} active, {} today, {} total",
        s.active, s.today, s.total
    );
    text
}

fn probes_text(probes: &[deep::Probe]) -> String {
    let mut text = String::from("probes:\n");
    if probes.is_empty() {
        text.push_str("  (nothing to probe)\n");
    }
    for p in probes {
        let outcome = match (&p.latency_ms, &p.error) {
            _ if p.inbound_only => "inbound only".to_owned(),
            (Some(ms), _) => format!("ok in {ms} ms"),
            (None, Some(why)) => format!("failed: {why}"),
            (None, None) => "unknown".to_owned(),
        };
        let _ = writeln!(text, "  {} {}: {outcome}", p.kind, p.name);
    }
    text
}
