//! `prometheus plugins pack|sign` (spec: plugin-system, Signed plugin
//! packages and provenance).

use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use pmt_plugins::manifest::Manifest;
use pmt_plugins::package;

use crate::output::print_raw;
use crate::plugins_host;

/// `pack <dir> [--out <file>]`: the package and its digest.
pub(super) fn pack(dir: &Path, out: Option<&Path>) -> Result<u8, Error> {
    let manifest = Manifest::read(dir, &plugins_host::running_version())?;
    let bytes = package::pack(dir)?;
    let out = out.map_or_else(
        || {
            PathBuf::from(format!(
                "{}-{}.{}",
                manifest.id,
                manifest.version,
                package::EXTENSION
            ))
        },
        Path::to_path_buf,
    );
    std::fs::write(&out, &bytes).map_err(|e| Error::io(&codes::PLUGINS_CATALOG, &out, &e))?;
    print_raw(&format!("{} {}\n", out.display(), package::digest(&bytes)))
}

/// `sign <package> --key <file>`: writes `<package>.minisig` over the
/// package digest.
pub(super) fn sign(package_path: &Path, key: &Path) -> Result<u8, Error> {
    let bytes = std::fs::read(package_path)
        .map_err(|e| Error::io(&codes::PLUGINS_CATALOG, package_path, &e))?;
    let secret =
        std::fs::read_to_string(key).map_err(|e| Error::io(&codes::PLUGINS_SIGNATURE, key, &e))?;
    let digest = package::digest(&bytes);
    let signature = package::sign(&digest, &secret)?;
    let out = package::signature_path(package_path);
    std::fs::write(&out, signature).map_err(|e| Error::io(&codes::PLUGINS_SIGNATURE, &out, &e))?;
    print_raw(&format!("{} signs {digest}\n", out.display()))
}
