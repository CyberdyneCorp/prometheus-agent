//! `prometheus runtimes` arguments (spec: distribution-and-update).

use clap::Subcommand;

/// `prometheus runtimes` subcommands.
#[derive(Debug, Subcommand)]
pub enum RuntimesCommand {
    /// Pinned and installed versions with their size and users.
    List,
    /// Download, verify and install the pinned versions.
    Install {
        /// Runtime names (node, cpython, uv, chromium), or `python@<version>`
        /// for the python bridge's managed CPython (3.11, 3.12, 3.13).
        #[arg(required = true, value_name = "NAME")]
        names: Vec<String>,
    },
    /// Remove every installed version no process is using.
    Remove {
        /// Runtime name.
        name: String,
    },
    /// Re-hash every installed file against its recorded digest (exit 1 on
    /// any mismatch).
    Verify,
    /// Remove versions neither this binary nor its `.prev` pins and no
    /// process uses, and partial downloads older than 24 hours.
    Gc,
    /// Report the store: location, platform, mirror, exclusions and the
    /// verification state of every installed version.
    Doctor,
    /// List and edit `runtimes.exclude` (runtimes nothing may install).
    Exclude {
        /// The exclude subcommand.
        #[command(subcommand)]
        command: ExcludeCommand,
    },
    /// Install this build's pins of the runtimes in use, then collect
    /// garbage (run by `prometheus update` with the new binary).
    #[command(hide = true)]
    Refresh,
}

/// `prometheus runtimes exclude` subcommands.
#[derive(Debug, Subcommand)]
pub enum ExcludeCommand {
    /// The excluded runtimes.
    List,
    /// Exclude a runtime.
    Add {
        /// Runtime name.
        name: String,
    },
    /// Allow a runtime again.
    Remove {
        /// Runtime name.
        name: String,
    },
}
