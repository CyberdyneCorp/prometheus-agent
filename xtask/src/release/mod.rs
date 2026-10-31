//! `cargo xtask release ...`: the release steps the Release workflow runs
//! around cargo-dist (spec: distribution-and-update, Release artifacts and
//! targets; Signature trust root; Installers):
//!
//! - `keygen`: a minisign key pair (the release primary/standby keys, or an
//!   ephemeral key for a dry run)
//! - `sbom`: the SPDX 2.3 JSON SBOM of the binary for one target
//! - `manifest`: `release-manifest.json` and `channel-<channel>.json` over
//!   the archives in a directory
//! - `installers`: `install.sh` / `install.ps1` with the trusted keys
//!   embedded
//! - `sign`: a detached `.minisig` for each file
//! - `verify`: every expected artifact present, matching the signed
//!   manifest, checksummed, signed and with an SBOM

mod installers;
mod manifest;
mod sbom;
mod sign;

use std::path::PathBuf;

use clap::Subcommand;

/// `cargo xtask release <step>`.
#[derive(Subcommand)]
pub(crate) enum Step {
    /// Generate a minisign key pair `<name>.key` (secret, unencrypted) and
    /// `<name>.pub`.
    Keygen {
        /// Output directory.
        #[arg(long)]
        out: PathBuf,
        /// File stem.
        #[arg(long)]
        name: String,
    },
    /// Write the SPDX 2.3 JSON SBOM of `prometheus` for a target.
    Sbom {
        /// Target triple.
        #[arg(long)]
        target: String,
        /// Output file.
        #[arg(long)]
        out: PathBuf,
    },
    /// Write `release-manifest.json` and `channel-<channel>.json` for the
    /// `prometheus-<target>.tar.gz` archives in a directory.
    Manifest {
        /// Directory holding the archives; the files are written there.
        #[arg(long)]
        dir: PathBuf,
        /// Release tag (`vX.Y.Z` or `vX.Y.Z-canary.<ts>`).
        #[arg(long)]
        tag: String,
        /// Commit the release was built from.
        #[arg(long)]
        commit: String,
    },
    /// Render `install.sh` and `install.ps1` with the trusted public keys.
    Installers {
        /// Output directory.
        #[arg(long)]
        out: PathBuf,
        /// Public key files to embed (default: the binary's primary and
        /// standby keys).
        #[arg(long = "key")]
        keys: Vec<PathBuf>,
    },
    /// Write `<file>.minisig` for each file.
    Sign {
        /// Environment variable holding the secret key file's text.
        #[arg(long)]
        key_env: String,
        /// Refuse a key that is not one of the binary's embedded keys.
        #[arg(long)]
        require_embedded: bool,
        /// Also write the signing key's public key here (for `verify`).
        #[arg(long)]
        public_key_out: Option<PathBuf>,
        /// Files to sign.
        files: Vec<PathBuf>,
    },
    /// Check a release directory: every target archive, its checksum,
    /// signature and SBOM, the signed manifest and channel index, and the
    /// signed installers.
    Verify {
        /// Release directory.
        #[arg(long)]
        dir: PathBuf,
        /// The public key the release was signed with.
        #[arg(long)]
        key: PathBuf,
    },
}

/// Runs one release step.
pub(crate) fn run(step: Step) -> Result<(), String> {
    match step {
        Step::Keygen { out, name } => sign::keygen(&out, &name),
        Step::Sbom { target, out } => sbom::write(&crate::repo_root(), &target, &out),
        Step::Manifest { dir, tag, commit } => manifest::write(&dir, &tag, &commit),
        Step::Installers { out, keys } => installers::render(&crate::repo_root(), &out, &keys),
        Step::Sign {
            key_env,
            require_embedded,
            public_key_out,
            files,
        } => {
            let key = std::env::var(&key_env).map_err(|_| format!("{key_env} is not set"))?;
            sign::sign_files(&key, require_embedded, public_key_out.as_deref(), &files)
        }
        Step::Verify { dir, key } => manifest::verify(&dir, &key),
    }
}
