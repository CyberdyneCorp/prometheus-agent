//! The `prometheus skills` command tree.

use std::path::PathBuf;

use clap::Subcommand;

/// `prometheus skills` subcommands.
#[derive(Debug, Subcommand)]
pub enum SkillsCommand {
    /// List the profile's skills with category, source and status (shadowed copies marked).
    List {
        /// Only ejected skills whose bundled origin changed in this release.
        #[arg(long)]
        outdated: bool,
    },
    /// Install a skill from the hub (official/<category>/<name>, <owner>/<repo>/<path>, skills-sh/..., clawhub:..., a SKILL.md URL...).
    Install {
        /// The skill identifier (omit with --locked).
        identifier: Option<String>,
        /// Override a non-dangerous block and replace an installed copy.
        #[arg(long)]
        force: bool,
        /// The skill name when the source gives none (a bare SKILL.md URL).
        #[arg(long, value_name = "N")]
        name: Option<String>,
        /// Reproduce exactly the skills in skills.lock; fail on any digest mismatch.
        #[arg(long, conflicts_with = "identifier")]
        locked: bool,
    },
    /// Remove a hub-installed skill.
    Uninstall {
        /// The skill.
        name: String,
    },
    /// List installable skills: the official catalog first, then the hub index and taps.
    Browse {
        /// Only this source (official, github, skills-sh, clawhub, lobehub, browse-sh, well-known, url).
        #[arg(long, value_name = "S")]
        source: Option<String>,
    },
    /// Search the hub (the cached index first, live sources only when it has no match).
    Search {
        /// What to look for.
        query: String,
        /// Only this source.
        #[arg(long, value_name = "S")]
        source: Option<String>,
    },
    /// Preview a hub skill (SKILL.md, files, scan verdict, upstream metadata) without installing it.
    Inspect {
        /// The skill identifier.
        identifier: String,
    },
    /// Report hub skills whose upstream changed, orphaned installs and unsafe recorded paths.
    Check {
        /// One skill.
        name: Option<String>,
    },
    /// Reinstall hub skills whose upstream changed (locally modified ones need --force).
    Update {
        /// One skill.
        name: Option<String>,
        /// Also replace locally modified skills.
        #[arg(long)]
        force: bool,
    },
    /// Re-scan every hub skill and quarantine dangerous ones.
    Audit,
    /// Accept the current files of a quarantined hub skill (re-scans and re-records its digest).
    Accept {
        /// The skill.
        name: String,
    },
    /// Sign a skill directory: writes SKILL.sig over its content digest.
    Sign {
        /// The skill directory.
        dir: PathBuf,
        /// A minisign secret key (default: `skills.signing_key`).
        #[arg(long, value_name = "PATH", conflicts_with = "keyless")]
        key: Option<PathBuf>,
        /// Sign with a Sigstore keyless identity (OpenID sign-in).
        #[arg(long)]
        keyless: bool,
    },
    /// Manage skill taps (GitHub repositories of skills).
    Tap {
        /// The tap subcommand.
        #[command(subcommand)]
        command: TapCommand,
    },
    /// Lint, scan and sign a skill, then open a pull request or registry submission.
    Publish {
        /// The skill directory.
        dir: PathBuf,
        /// Where to publish.
        #[arg(long, value_enum)]
        to: PublishTo,
        /// The repository the pull request targets (GitHub).
        #[arg(long, value_name = "owner/repo")]
        repo: Option<String>,
    },
    /// Export or import the hub skills and taps of the profile.
    Snapshot {
        /// The snapshot subcommand.
        #[command(subcommand)]
        command: SnapshotCommand,
    },
    /// Remove a trusted project root.
    Untrust {
        /// The repository root.
        root: PathBuf,
    },
    /// Show one skill: where it comes from, its status, its files and its SKILL.md.
    Show {
        /// The skill.
        name: String,
    },
    /// Enable a skill (from the next session).
    Enable {
        /// The skill.
        name: String,
        /// Only on this surface or adapter (`skills.platform_disabled.<P>`).
        #[arg(long, value_name = "P")]
        platform: Option<String>,
    },
    /// Disable a skill (from the next session); `prometheus-agent` cannot be disabled.
    Disable {
        /// The skill.
        name: String,
        /// Only on this surface or adapter (`skills.platform_disabled.<P>`).
        #[arg(long, value_name = "P")]
        platform: Option<String>,
    },
    /// Check skills against the authoring rules (all of the profile's when no path is given).
    Lint {
        /// Skill directories, SKILL.md files or directories holding skills.
        paths: Vec<PathBuf>,
        /// Exit 1 when any finding exists.
        #[arg(long)]
        strict: bool,
    },
    /// Load the skills of a git checkout (`.prometheus/skills`, `.agents/skills`) in new sessions.
    Trust {
        /// The repository root.
        root: PathBuf,
    },
    /// Copy a bundled skill into the profile as an editable copy that shadows it.
    Eject {
        /// The bundled skill.
        name: String,
    },
    /// Show how an ejected copy differs from the current bundled version.
    Diff {
        /// The ejected skill.
        name: String,
    },
    /// Remove an ejected copy so the name resolves to the bundled skill again, or restore a hub skill to its locked content.
    Reset {
        /// The ejected or hub skill.
        name: String,
    },
    /// Report skills that do not load (path, line, reason) and quarantined project skills.
    Doctor,
    /// Rescan every skill root and bundle; report added, removed and changed skills since the last reload.
    Reload,
    /// Manage skill bundles (`<profile>/skill-bundles/<slug>.toml`).
    Bundles {
        /// The bundles subcommand.
        #[command(subcommand)]
        command: BundlesCommand,
    },
    /// Review skill writes staged by `skills.write_approval` (default: list them).
    Pending {
        /// The pending subcommand.
        #[command(subcommand)]
        command: Option<PendingCommand>,
    },
    /// Hide every bundled skill from this profile across upgrades (`prometheus-agent` stays).
    OptOut {
        /// Also delete ejected copies still identical to their bundled origin.
        #[arg(long)]
        remove: bool,
        /// Do not ask before deleting.
        #[arg(long)]
        yes: bool,
    },
    /// Show bundled skills again (the default).
    OptIn,
    /// List ejected skills whose content differs from their recorded origin.
    ListModified,
    /// Re-record the provenance of official catalog installs that match the embedded copy.
    RepairOfficial {
        /// An official skill name, or `all` for every official skill the profile holds.
        target: String,
        /// Replace missing or modified copies (old copies move to `skills/.backups/`).
        #[arg(long)]
        restore: bool,
        /// Do not ask before restoring.
        #[arg(long)]
        yes: bool,
    },
}

/// `prometheus skills tap` subcommands.
#[derive(Debug, Subcommand)]
pub enum TapCommand {
    /// List the taps.
    List,
    /// Add a tap.
    Add {
        /// `owner/repo`.
        repo: String,
        /// Where its skills live in the repository.
        #[arg(long, value_name = "P")]
        path: Option<String>,
    },
    /// Remove a tap.
    Remove {
        /// `owner/repo`.
        repo: String,
    },
}

/// `prometheus skills publish --to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PublishTo {
    /// A pull request to a GitHub repository.
    Github,
    /// The ClawHub registry.
    Clawhub,
}

/// `prometheus skills snapshot` subcommands.
#[derive(Debug, Subcommand)]
pub enum SnapshotCommand {
    /// Write the snapshot to a file (`-` for stdout).
    Export {
        /// The file, or `-`.
        file: String,
    },
    /// Add the taps and install every skill of a snapshot.
    Import {
        /// The snapshot file.
        file: PathBuf,
        /// Reinstall skills already present.
        #[arg(long)]
        force: bool,
    },
}

/// `prometheus skills bundles` subcommands.
#[derive(Debug, Subcommand)]
pub enum BundlesCommand {
    /// List the bundles and the files that do not load.
    List,
    /// Show one bundle.
    Show {
        /// The bundle's command name.
        slug: String,
    },
    /// Write a bundle.
    Create {
        /// The bundle's command name (`/<slug>`).
        slug: String,
        /// A skill to load; repeatable, in order.
        #[arg(long = "skill", value_name = "S", required = true)]
        skills: Vec<String>,
        /// One line for listings.
        #[arg(long, value_name = "D")]
        description: Option<String>,
        /// Text added after the skills.
        #[arg(long, value_name = "I")]
        instruction: Option<String>,
        /// Replace an existing bundle.
        #[arg(long)]
        force: bool,
    },
    /// Delete a bundle.
    Delete {
        /// The bundle's command name.
        slug: String,
    },
    /// Rescan the bundle directory; report added, changed and removed bundles.
    Reload,
    /// Install the missing skills of a bundle that name a hub `source`.
    Install {
        /// The bundle's command name.
        slug: String,
    },
}

/// `prometheus skills pending` subcommands.
#[derive(Debug, Subcommand)]
pub enum PendingCommand {
    /// List staged writes with a one-line gist.
    List,
    /// Show the unified diff of a staged write.
    Diff {
        /// The pending id.
        id: String,
    },
    /// Apply a staged write (or `all`) through the normal validation and scan.
    Approve {
        /// The pending id, or `all`.
        target: String,
    },
    /// Drop a staged write (or `all`).
    Reject {
        /// The pending id, or `all`.
        target: String,
    },
}
