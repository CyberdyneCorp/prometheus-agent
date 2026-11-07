//! The hardening every container starts with (spec: execution-environments,
//! Container hardening is never silently dropped).
//!
//! Each [`Flag`] is one hardening measure with the `allow_degraded` feature
//! that may waive it and the words a runtime's error uses when it rejects
//! the flag. A rejection is mapped back to its flag: without a matching
//! `allow_degraded` entry creation fails naming it, with one the flag is
//! dropped and a WARN is logged on every start.

use pmt_config::schema::{ContainerConfig, DegradedFeature};
use pmt_core::error::{Error, codes};

/// One hardening measure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flag {
    /// Its name in errors and logs (`no_new_privileges`).
    pub name: &'static str,
    /// The `create` arguments.
    pub args: Vec<String>,
    /// The `allow_degraded` feature that waives it (`all` always does).
    pub feature: DegradedFeature,
    /// Lower-case words of a runtime error rejecting it.
    pub rejected_by: &'static [&'static str],
}

/// The capabilities kept after `--cap-drop ALL`.
pub const KEPT_CAPABILITIES: &[&str] = &["CHOWN", "DAC_OVERRIDE", "FOWNER"];

/// Every hardening flag for `config`, in `create` order.
#[must_use]
pub fn flags(config: &ContainerConfig) -> Vec<Flag> {
    let mut cap_args = vec!["--cap-drop=ALL".to_owned()];
    cap_args.extend(KEPT_CAPABILITIES.iter().map(|c| format!("--cap-add={c}")));
    vec![
        Flag {
            name: "cap_drop",
            args: cap_args,
            feature: DegradedFeature::CapDrop,
            rejected_by: &["cap-drop", "cap-add", "capabilit"],
        },
        Flag {
            name: "no_new_privileges",
            args: vec!["--security-opt=no-new-privileges".to_owned()],
            feature: DegradedFeature::NoNewPrivileges,
            rejected_by: &["no-new-privileges", "no_new_privs", "nonewprivileges"],
        },
        Flag {
            name: "read_only_rootfs",
            args: vec!["--read-only".to_owned()],
            feature: DegradedFeature::ReadOnlyRootfs,
            rejected_by: &["read-only", "readonly", "read only"],
        },
        Flag {
            name: "tmpfs",
            args: vec![
                // Docker mounts a tmpfs `noexec` unless told otherwise; the
                // scratch `/tmp` runs what commands build or upload there.
                "--tmpfs=/tmp:rw,exec,nosuid,size=512m".to_owned(),
                "--tmpfs=/var/tmp:rw,noexec,nosuid,size=256m".to_owned(),
            ],
            feature: DegradedFeature::All,
            rejected_by: &["tmpfs"],
        },
        Flag {
            name: "pids_limit",
            args: vec![format!("--pids-limit={}", config.pids_limit)],
            feature: DegradedFeature::All,
            rejected_by: &["pids"],
        },
        Flag {
            name: "cpus",
            args: vec![format!("--cpus={}", config.cpus)],
            feature: DegradedFeature::All,
            rejected_by: &["cpus", "nanocpus", "cpu quota", "cpu.max", "cpu cfs"],
        },
        Flag {
            name: "memory",
            args: vec![format!("--memory={}b", config.memory_bytes)],
            feature: DegradedFeature::All,
            rejected_by: &["memory"],
        },
    ]
}

impl Flag {
    /// Whether `allowed` waives this flag.
    #[must_use]
    pub fn waived_by(&self, allowed: &[DegradedFeature]) -> bool {
        allowed.contains(&self.feature) || allowed.contains(&DegradedFeature::All)
    }
}

/// The flag of `flags` a runtime error names, if any.
#[must_use]
pub fn rejected<'a>(flags: &'a [Flag], stderr: &str) -> Option<&'a Flag> {
    let lower = stderr.to_ascii_lowercase();
    flags
        .iter()
        .find(|flag| flag.rejected_by.iter().any(|word| lower.contains(word)))
}

/// The error for a rejected flag no `allow_degraded` entry waives.
#[must_use]
pub fn unsupported(flag: &Flag) -> Error {
    Error::new(
        &codes::TERMINAL_CONTAINER_HARDENING,
        format!("container hardening '{}' unsupported by runtime", flag.name),
    )
    .with_hint(format!(
        "use a runtime that supports it, or set security.sandbox.allow_degraded = [\"{}\"] to run without it",
        flag.feature.as_str()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runtime_error_maps_back_to_the_flag_it_names() {
        let flags = flags(&ContainerConfig::default());
        let flag = rejected(
            &flags,
            "Error response from daemon: invalid --security-opt: no-new-privileges unsupported",
        )
        .unwrap();
        assert_eq!(flag.name, "no_new_privileges");
        assert!(rejected(&flags, "image not found").is_none());
        assert_eq!(
            unsupported(flag).message(),
            "container hardening 'no_new_privileges' unsupported by runtime"
        );
    }

    #[test]
    fn only_the_matching_feature_or_all_waives_a_flag() {
        let flags = flags(&ContainerConfig::default());
        let nnp = flags
            .iter()
            .find(|f| f.name == "no_new_privileges")
            .unwrap();
        assert!(!nnp.waived_by(&[DegradedFeature::CapDrop]));
        assert!(nnp.waived_by(&[DegradedFeature::NoNewPrivileges]));
        assert!(nnp.waived_by(&[DegradedFeature::All]));
        let pids = flags.iter().find(|f| f.name == "pids_limit").unwrap();
        assert_eq!(pids.args, ["--pids-limit=2048"]);
        assert!(!pids.waived_by(&[DegradedFeature::NoNewPrivileges]));
    }
}
