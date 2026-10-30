//! Protected instruction files (spec: security-boundaries, Protected
//! instruction files configuration; execution-environments, Write guards).
//!
//! Instruction files steer every later session, so a prompt-injected write
//! to one persists the injection. Agent writes to them keep needing a human
//! approval in every approval mode, including `approvals.mode = "off"`
//! (`--yolo`): [`ProtectedFiles::requires_approval`] takes no mode on
//! purpose. Turning the protection off is a hardening downgrade, logged at
//! WARN on every start and reported by `prometheus doctor security`.

use std::path::{Component, Path, PathBuf};

use glob::{MatchOptions, Pattern};
use pmt_config::schema::SecurityConfig;
use pmt_core::doctor::{Check, Status};
use pmt_core::error::{Error, codes};

/// Built-in protected basenames (matched case-insensitively).
pub const BUILTIN_FILES: &[&str] = &[
    "AGENTS.md",
    "CLAUDE.md",
    "SOUL.md",
    "IDENTITY.md",
    ".cursorrules",
];

/// Directory name of project-local Prometheus configuration; everything
/// under it is protected.
pub const PROJECT_CONFIG_DIR: &str = ".prometheus";

/// Logged at WARN on every start while the protection is off.
pub const DISABLED_WARNING: &str = "security.protected_instruction_files = false: agent writes to instruction files (AGENTS.md, CLAUDE.md, SOUL.md, ...) no longer need human approval";

const MATCH: MatchOptions = MatchOptions {
    case_sensitive: false,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

/// Why a write needs human approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Protected {
    /// The path the rule matched (the written path or its resolved target).
    pub path: PathBuf,
    /// The matching pattern, or `.prometheus/` for project configuration.
    pub rule: String,
}

impl Protected {
    /// The reason shown in the approval prompt.
    #[must_use]
    pub fn reason(&self) -> String {
        format!(
            "{} is a protected instruction file ({}); writes always need human approval",
            self.path.display(),
            self.rule
        )
    }
}

/// The protected-file rules of one session.
#[derive(Debug, Clone)]
pub struct ProtectedFiles {
    enabled: bool,
    patterns: Vec<(String, Pattern)>,
}

impl ProtectedFiles {
    /// The rules from `security.protected_instruction_files` and
    /// `security.protected_instruction_extra_patterns`.
    ///
    /// # Errors
    ///
    /// `security.invalid_pattern` naming an extra pattern that is not a
    /// basename glob.
    pub fn from_config(security: &SecurityConfig) -> Result<Self, Error> {
        let builtin = BUILTIN_FILES.iter().map(|name| (*name).to_owned());
        let patterns = builtin
            .chain(
                security
                    .protected_instruction_extra_patterns
                    .iter()
                    .cloned(),
            )
            .map(|raw| compile(&raw).map(|pattern| (raw, pattern)))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            enabled: security.protected_instruction_files,
            patterns,
        })
    }

    /// Whether an agent write to `path` needs human approval, whatever the
    /// approval mode. Symbolic links are resolved too, so a link to a
    /// protected file (or into `.prometheus/`) is protected.
    #[must_use]
    pub fn requires_approval(&self, path: &Path) -> Option<Protected> {
        if !self.enabled {
            return None;
        }
        candidates(path)
            .into_iter()
            .find_map(|candidate| self.rule_for(&candidate))
    }

    fn rule_for(&self, path: &Path) -> Option<Protected> {
        let protected = |rule: &str| Protected {
            path: path.to_owned(),
            rule: rule.to_owned(),
        };
        let in_project_config = path.parent().is_some_and(|dir| {
            dir.components().any(|c| match c {
                Component::Normal(name) => name.eq_ignore_ascii_case(PROJECT_CONFIG_DIR),
                _ => false,
            })
        });
        if in_project_config {
            return Some(protected(&format!("{PROJECT_CONFIG_DIR}/")));
        }
        let name = path.file_name()?.to_string_lossy();
        self.patterns
            .iter()
            .find(|(_, pattern)| pattern.matches_with(&name, MATCH))
            .map(|(raw, _)| protected(raw))
    }

    /// The `doctor security` entry: a downgrade while the protection is
    /// off.
    #[must_use]
    pub fn doctor_check(&self) -> Check {
        if self.enabled {
            Check::new(
                "security",
                "protected_instruction_files",
                Status::Ok,
                format!(
                    "instruction file writes need approval ({} patterns)",
                    self.patterns.len()
                ),
            )
        } else {
            Check::new(
                "security",
                "protected_instruction_files",
                Status::Warn,
                DISABLED_WARNING,
            )
            .with_hint("prometheus config set security.protected_instruction_files true")
        }
    }
}

/// Logs [`DISABLED_WARNING`] when the protection is off (call on every
/// start).
pub fn warn_if_disabled(security: &SecurityConfig) {
    if !security.protected_instruction_files {
        tracing::warn!("{DISABLED_WARNING}");
    }
}

fn compile(raw: &str) -> Result<Pattern, Error> {
    let invalid = |why: &str| {
        Error::new(
            &codes::SECURITY_INVALID_PATTERN,
            format!("security.protected_instruction_extra_patterns: '{raw}' {why}"),
        )
    };
    if raw.is_empty() || raw.contains(['/', '\\']) {
        return Err(invalid("must be a basename glob such as \"*.mdc\""));
    }
    Pattern::new(raw).map_err(|e| invalid(&format!("is not a valid glob: {e}")))
}

/// The written path, plus its resolved forms when they exist on disk.
fn candidates(path: &Path) -> Vec<PathBuf> {
    let mut all = vec![path.to_owned()];
    if let Ok(target) = std::fs::canonicalize(path) {
        all.push(target);
    } else if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && let Ok(dir) = std::fs::canonicalize(if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        })
    {
        all.push(dir.join(name));
    }
    all
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use pmt_config::schema::SecurityConfig;
    use pmt_core::doctor::Status;

    use super::ProtectedFiles;

    fn rules(extra: &[&str], enabled: bool) -> ProtectedFiles {
        let security = SecurityConfig {
            protected_instruction_files: enabled,
            protected_instruction_extra_patterns: extra.iter().map(|s| (*s).to_owned()).collect(),
            ..SecurityConfig::default()
        };
        ProtectedFiles::from_config(&security).unwrap()
    }

    #[test]
    fn builtin_files_and_project_config_are_protected_case_insensitively() {
        let rules = rules(&[], true);
        for path in [
            "AGENTS.md",
            "sub/agents.md",
            "/repo/CLAUDE.md",
            "SOUL.md",
            "IDENTITY.md",
            ".cursorrules",
            "proj/.prometheus/config.toml",
            "proj/.Prometheus/hooks/x.sh",
        ] {
            assert!(rules.requires_approval(Path::new(path)).is_some(), "{path}");
        }
        for path in ["README.md", "src/agents.rs", ".prometheus", "AGENTS.md.bak"] {
            assert!(rules.requires_approval(Path::new(path)).is_none(), "{path}");
        }
    }

    #[test]
    fn extra_patterns_extend_the_set_and_must_be_basename_globs() {
        let rules = rules(&["*.mdc"], true);
        let hit = rules
            .requires_approval(Path::new(".cursor/rules/style.mdc"))
            .unwrap();
        assert_eq!(hit.rule, "*.mdc");
        assert!(hit.reason().contains("always need human approval"));

        for bad in ["rules/*.mdc", "", "[", "a\\b"] {
            let security = SecurityConfig {
                protected_instruction_extra_patterns: vec![bad.to_owned()],
                ..SecurityConfig::default()
            };
            let err = ProtectedFiles::from_config(&security).unwrap_err();
            assert_eq!(err.code().code, "security.invalid_pattern", "{bad}");
            assert_eq!(err.exit_code(), 78);
        }
    }

    #[cfg(unix)]
    #[test]
    fn links_to_protected_files_are_protected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "").unwrap();
        let link = dir.path().join("notes.txt");
        std::os::unix::fs::symlink(dir.path().join("AGENTS.md"), &link).unwrap();
        assert!(rules(&[], true).requires_approval(&link).is_some());

        let config = dir.path().join(".prometheus");
        std::fs::create_dir(&config).unwrap();
        let alias = dir.path().join("cfg");
        std::os::unix::fs::symlink(&config, &alias).unwrap();
        assert!(
            rules(&[], true)
                .requires_approval(&alias.join("new.toml"))
                .is_some()
        );
    }

    #[test]
    fn disabling_is_a_reported_downgrade() {
        let off = rules(&["*.mdc"], false);
        assert!(off.requires_approval(Path::new("AGENTS.md")).is_none());
        assert_eq!(off.doctor_check().status, Status::Warn);
        assert_eq!(rules(&[], true).doctor_check().status, Status::Ok);
    }
}
