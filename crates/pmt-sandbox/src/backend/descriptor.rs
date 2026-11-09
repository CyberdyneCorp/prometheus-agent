//! The isolation descriptor every terminal backend declares (spec:
//! execution-environments, Backend isolation descriptor).
//!
//! Approval and file-tool decisions read only this descriptor, never the
//! backend name, so a plugin backend gets exactly the treatment its declared
//! isolation earns.

use pmt_core::error::{Error, codes};
use serde::Deserialize;

/// What separates a backend's commands from the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// The host, confined by the OS sandbox.
    OsSandbox,
    /// A container on this host.
    Container,
    /// Another machine.
    Remote,
}

impl Isolation {
    /// The descriptor spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OsSandbox => "os_sandbox",
            Self::Container => "container",
            Self::Remote => "remote",
        }
    }
}

/// A backend's declared isolation.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    /// What separates commands from the host.
    pub isolation: Isolation,
    /// Whether paths are host paths (file tools check capabilities then).
    pub uses_host_filesystem: bool,
    /// Where a session starts: an absolute path or `~`.
    pub default_cwd: String,
    /// Whether an ephemeral environment is private to one session.
    pub session_isolated_when_ephemeral: bool,
    /// Whether the environment outlives the session.
    pub persistent: bool,
}

impl Descriptor {
    /// The `local` backend.
    #[must_use]
    pub fn local() -> Self {
        Self {
            isolation: Isolation::OsSandbox,
            uses_host_filesystem: true,
            default_cwd: "~".to_owned(),
            session_isolated_when_ephemeral: false,
            persistent: true,
        }
    }

    /// The `container` backend: `/workspace` when the workspace is mounted,
    /// `/` otherwise.
    #[must_use]
    pub fn container(mount_workspace: bool, persistent: bool) -> Self {
        Self {
            isolation: Isolation::Container,
            uses_host_filesystem: false,
            default_cwd: if mount_workspace { "/workspace" } else { "/" }.to_owned(),
            session_isolated_when_ephemeral: true,
            persistent,
        }
    }

    /// The `ssh` backend.
    #[must_use]
    pub fn ssh() -> Self {
        Self {
            isolation: Isolation::Remote,
            uses_host_filesystem: false,
            default_cwd: "~".to_owned(),
            session_isolated_when_ephemeral: false,
            persistent: true,
        }
    }

    /// The `singularity` backend: the profile's instance, whose overlay
    /// outlives sessions; commands start in the contained home.
    #[must_use]
    pub fn singularity() -> Self {
        Self {
            isolation: Isolation::Container,
            uses_host_filesystem: false,
            default_cwd: "~".to_owned(),
            session_isolated_when_ephemeral: false,
            persistent: true,
        }
    }

    /// A plugin's descriptor from its JSON (`describe`).
    ///
    /// # Errors
    ///
    /// `terminal.backend_descriptor` naming the missing field or invalid
    /// value.
    pub fn from_json(backend: &str, json: &str) -> Result<Self, Error> {
        let invalid = |why: String| {
            Error::new(
                &codes::TERMINAL_BACKEND_DESCRIPTOR,
                format!("terminal backend '{backend}' rejected: {why}"),
            )
        };
        let descriptor: Self = serde_json::from_str(json)
            .map_err(|err| invalid(format!("invalid descriptor: {err}")))?;
        descriptor.validate().map_err(invalid)?;
        Ok(descriptor)
    }

    fn validate(&self) -> Result<(), String> {
        let cwd = self.default_cwd.as_str();
        // `/` (Unix) and `C:\` style roots are both absolute on a remote.
        let absolute = cwd.starts_with('/')
            || (cwd.len() >= 3
                && cwd.as_bytes()[1] == b':'
                && matches!(cwd.as_bytes()[2], b'\\' | b'/'));
        if cwd != "~" && !absolute {
            return Err("default_cwd must be absolute or '~'".to_owned());
        }
        if self.uses_host_filesystem && self.isolation != Isolation::OsSandbox {
            return Err(format!(
                "uses_host_filesystem requires isolation 'os_sandbox', not '{}'",
                self.isolation.as_str()
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"{"isolation":"remote","uses_host_filesystem":false,"default_cwd":"/home/sandbox","session_isolated_when_ephemeral":true,"persistent":false}"#;

    #[test]
    fn a_complete_descriptor_parses() {
        let d = Descriptor::from_json("acme", VALID).unwrap();
        assert_eq!(d.isolation, Isolation::Remote);
        assert_eq!(d.default_cwd, "/home/sandbox");
    }

    #[test]
    fn a_relative_default_cwd_is_rejected() {
        let json = VALID.replace("/home/sandbox", "work");
        let err = Descriptor::from_json("acme", &json).unwrap_err();
        assert_eq!(err.code(), &codes::TERMINAL_BACKEND_DESCRIPTOR);
        assert_eq!(
            err.message(),
            "terminal backend 'acme' rejected: default_cwd must be absolute or '~'"
        );
    }

    #[test]
    fn a_missing_field_or_an_invalid_value_is_rejected() {
        let missing = VALID.replace(r#""persistent":false"#, r#""persistent2":false"#);
        let err = Descriptor::from_json("acme", &missing).unwrap_err();
        assert!(err.message().contains("persistent"), "{}", err.message());
        let invalid = VALID.replace(r#""remote""#, r#""vm""#);
        let err = Descriptor::from_json("acme", &invalid).unwrap_err();
        assert!(
            err.message().contains("unknown variant `vm`"),
            "{}",
            err.message()
        );
        let host_fs = VALID.replace(
            r#""uses_host_filesystem":false"#,
            r#""uses_host_filesystem":true"#,
        );
        assert!(Descriptor::from_json("acme", &host_fs).is_err());
    }
}
