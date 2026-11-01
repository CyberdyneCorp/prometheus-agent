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
}
