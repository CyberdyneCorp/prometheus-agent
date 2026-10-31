//! Renders `distrib/install.sh` and `distrib/install.ps1` with the trusted
//! minisign public keys embedded (spec: distribution-and-update,
//! Installers; security-boundaries, Signed releases).

use std::path::{Path, PathBuf};

const PLACEHOLDER: &str = "@@RELEASE_KEYS@@";
const TEMPLATES: [&str; 2] = ["install.sh", "install.ps1"];

/// Writes the rendered installers to `out`; `keys` defaults to the
/// binary's primary and standby keys.
pub(super) fn render(root: &Path, out: &Path, keys: &[PathBuf]) -> Result<(), String> {
    let defaults = [
        root.join("crates/pmt-update/keys/primary.pub"),
        root.join("crates/pmt-update/keys/standby.pub"),
    ];
    let keys = if keys.is_empty() { &defaults[..] } else { keys };
    let lines = keys
        .iter()
        .map(|path| key_line(path))
        .collect::<Result<Vec<_>, _>>()?
        .join("\n");
    std::fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    for name in TEMPLATES {
        let template = root.join("distrib").join(name);
        let text = std::fs::read_to_string(&template)
            .map_err(|e| format!("{}: {e}", template.display()))?;
        if !text.contains(PLACEHOLDER) {
            return Err(format!("{} has no {PLACEHOLDER}", template.display()));
        }
        let target = out.join(name);
        std::fs::write(&target, text.replace(PLACEHOLDER, &lines))
            .map_err(|e| format!("{}: {e}", target.display()))?;
        set_executable(&target)?;
        println!("wrote {}", target.display());
    }
    Ok(())
}

/// The base64 line of a minisign `.pub` file.
fn key_line(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    pmt_update::TrustRoot::from_keys(&[("installer", &text)])
        .map_err(|e| format!("{}: {e}", path.display()))?;
    text.lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("{}: empty key file", path.display()))
}

fn set_executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
