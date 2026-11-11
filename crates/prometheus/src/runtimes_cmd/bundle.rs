//! `prometheus runtimes install --from-bundle <file>` (spec:
//! distribution-and-update, Offline bundles; Persistent runtime opt-outs):
//! installs runtimes from an offline bundle with no network access. The
//! bundle's runtime index must carry a valid release signature, and each
//! archive must match this build's own pinned digest, so a bundle can
//! never install anything this binary would not download itself.

use std::path::Path;

use pmt_config::daemon::DaemonConfig;
use pmt_core::error::Error;
use pmt_update::TrustRoot;
use pmt_update::bundle::{self, RUNTIME_INDEX_FILE};
use pmt_update::manifest::SIGNATURE_SUFFIX;
use pmt_update::runtimes::Store;

use crate::context::Context;
use crate::model_cmd::block_on;

/// Installs `names` (default: every runtime in the bundle) from `file`.
///
/// # Errors
///
/// `update.failed` (unreadable bundle, missing runtime), `update.signature`
/// (77), `runtime.excluded` (1) for an excluded name given explicitly,
/// `runtime.digest_mismatch` (77) for an archive this build does not pin.
pub(super) fn install(
    ctx: &Context<'_>,
    store: &Store,
    config: &DaemonConfig,
    names: &[String],
    file: &Path,
) -> Result<u8, Error> {
    let opened = bundle::open(file, &ctx.home()?.root().join("updates").join("staging"))?;
    let tag = opened.manifest.version.to_string();
    let index = format!("{tag}/{RUNTIME_INDEX_FILE}");
    let read = |path: &str| {
        std::fs::read(opened.dir.join(path)).map_err(|e| {
            Error::new(
                &pmt_core::error::codes::UPDATE_FAILED,
                format!("the bundle has no {path}: {e}"),
            )
        })
    };
    let signature =
        String::from_utf8_lossy(&read(&format!("{index}{SIGNATURE_SUFFIX}"))?).into_owned();
    TrustRoot::embedded().verify(RUNTIME_INDEX_FILE, &read(&index)?, &signature)?;
    let excluded = |name: &str| config.runtimes.exclude.iter().any(|e| e == name);
    let wanted: Vec<String> = if names.is_empty() {
        let (skipped, kept): (Vec<String>, Vec<String>) = opened
            .manifest
            .runtimes
            .iter()
            .cloned()
            .partition(|name| excluded(name));
        for name in skipped {
            eprintln!("{name} skipped: excluded by runtimes.exclude");
        }
        kept
    } else {
        if let Some(name) = names.iter().find(|n| excluded(n)) {
            return Err(pmt_update::runtimes::install::excluded(name));
        }
        names.to_vec()
    };
    let installer = super::installer(ctx, store.clone(), config).with_bundle(&opened.dir);
    for name in &wanted {
        let runtime = block_on(installer.install(name))??;
        println!(
            "{name} {} installed at {} (from the bundle)",
            runtime.record.version,
            runtime.dir.display()
        );
    }
    Ok(0)
}
