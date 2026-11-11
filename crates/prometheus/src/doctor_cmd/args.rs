//! `prometheus doctor` arguments (spec: cli).

use clap::Args;

/// `prometheus doctor` arguments.
#[derive(Debug, Clone, Args)]
#[allow(clippy::struct_excessive_bools)] // independent switches of one command line
pub struct DoctorArgs {
    /// Only this group: home, config, secrets, store, daemon, sandbox,
    /// terminal, runtimes, plugins, observability, network, providers or
    /// security.
    pub group: Option<String>,
    /// Perform the enumerated repairs (permissions, stale run files, the
    /// full-text index, pending config migrations) and print each.
    #[arg(long)]
    pub fix: bool,
    /// Allow network probes (provider reachability, IPv6).
    #[arg(long)]
    pub online: bool,
    /// Treat warnings as failures; `doctor security --strict` exits 77 on
    /// any downgrade.
    #[arg(long)]
    pub strict: bool,
    /// Check every profile of the home.
    #[arg(long)]
    pub all_profiles: bool,
    /// With `network`: print the hosts an egress proxy must allow for every
    /// profile (providers, MCP servers, releases and runtimes), one per
    /// line, for the image's `compose.isolated.yaml`.
    #[arg(long, requires = "group")]
    pub allowlist: bool,
    /// Silence the startup warning of a supply-chain advisory (added to
    /// `security.acked_advisories`); `doctor security` still shows it.
    #[arg(long, value_name = "ADVISORY_ID")]
    pub ack: Option<String>,
}
