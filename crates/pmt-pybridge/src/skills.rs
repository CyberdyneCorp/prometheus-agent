//! Python skill scripts (spec: python-bridge, Python skill scripts;
//! skills, Skill scripts run sandboxed with managed runtimes): a skill
//! declaring `metadata.prometheus.python` (`requires_python`,
//! `requirements`) gets its own pinned environment at
//! `<profile>/python/skills/<skill>-<lock_digest12>/`, built with the
//! plugin rules (upper bounds, a hash lock). Its scripts run under the
//! session's sandbox, not in a bridge sidecar, so no plugin environment is
//! ever on their path.

use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::env::{self, Env};
use crate::runtime::{Interpreter, Uv};

/// A skill's `python` declaration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillPython {
    /// `requires_python` as written (checked against the interpreter).
    pub requires_python: Option<String>,
    /// Its requirements.
    pub requirements: Vec<String>,
}

impl SkillPython {
    /// The declaration in a skill's `python` frontmatter field.
    #[must_use]
    pub fn of(field: &Value) -> Option<Self> {
        let table = field.as_object()?;
        Some(Self {
            requires_python: table
                .get("requires_python")
                .and_then(Value::as_str)
                .map(str::to_owned),
            requirements: table
                .get("requirements")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    /// Whether `version` (`3.12.11`) satisfies `requires_python`
    /// (`>=3.11`, `>=3.11,<3.14`; anything unreadable is accepted).
    #[must_use]
    pub fn accepts(&self, version: &str) -> bool {
        self.requires_python
            .as_deref()
            .is_none_or(|range| pmt_plugins::python::api::admits(range, version))
    }
}

/// `<profile>/python/skills`.
#[must_use]
pub fn root(profile_dir: &Path) -> PathBuf {
    profile_dir.join("python").join("skills")
}

/// The pinned environment of `skill`, resolving its lock the first time
/// (network) and building it when missing.
///
/// # Errors
///
/// `bridge.requirements` for an unbounded requirement or an interpreter
/// outside `requires_python`; `bridge.environment` from uv.
pub fn ensure(
    uv: &Uv,
    python: &Interpreter,
    root: &Path,
    skill: &str,
    declared: &SkillPython,
    allow_unbounded: bool,
) -> Result<Env, Error> {
    if !declared.accepts(&python.version) {
        return Err(Error::new(
            &codes::BRIDGE_REQUIREMENTS,
            format!(
                "skill '{skill}' requires Python {}; the managed interpreter is {}",
                declared.requires_python.as_deref().unwrap_or(""),
                python.version
            ),
        ));
    }
    env::check_bounds(&declared.requirements, allow_unbounded)?;
    let name: String = skill
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let wanted = crate::hex(&Sha256::digest(
        format!("{}\n{}", python.version, declared.requirements.join("\n")).as_bytes(),
    ));
    let lock_dir = root
        .join(".locks")
        .join(format!("{name}-{}", &wanted[..12]));
    let lock = lock_dir.join(env::LOCK_FILE);
    if !lock.is_file() {
        env::lock(uv, python, &declared.requirements, &lock_dir)?;
    }
    let built = env::ensure(uv, python, root, &name, &lock, false)?;
    pip_shim(uv, &built);
    Ok(built)
}

/// `pip` in a skill environment: uv's pip interface bound to that
/// environment (uv environments carry no pip of their own).
fn pip_shim(uv: &Uv, built: &Env) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let pip = built.bin.join("pip");
        if pip.exists() {
            return;
        }
        let script = format!(
            "#!/bin/sh\nUV_NO_CONFIG=1 UV_CACHE_DIR=\"${{TMPDIR:-/tmp}}/uv-cache\" exec '{}' pip \"$@\" --python '{}'\n",
            uv.exe.display(),
            built.python.display()
        );
        if std::fs::write(&pip, script).is_ok() {
            let _ = std::fs::set_permissions(&pip, std::fs::Permissions::from_mode(0o755));
        }
    }
    #[cfg(not(unix))]
    let _ = (uv, built);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_declaration_is_read_and_checked() {
        let field =
            serde_json::json!({"requires_python": ">=3.11", "requirements": ["pypdf>=4,<5"]});
        let declared = SkillPython::of(&field).unwrap();
        assert_eq!(declared.requirements, ["pypdf>=4,<5"]);
        assert!(declared.accepts("3.12.11"));
        assert!(!declared.accepts("3.10.4"));
    }
}
