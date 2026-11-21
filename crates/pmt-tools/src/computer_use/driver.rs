//! The driver seam (spec: computer-use, Permission modes are fixed per
//! runtime; Driver runtime management and identity): what the tool needs
//! from a desktop driver, independent of how it is reached.
//!
//! The `cua` backend speaks MCP over stdio to `cua-driver` (the binary
//! implements [`Launcher`] with it); tests use an in-process fake. A
//! [`Driver`] is one live runtime whose permission mode was fixed when it
//! was launched: dropping it stops the runtime.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::dispatch::SeamFuture;

/// A driver runtime's permission mode, immutable for its lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionMode {
    /// The machine-wide default: actions are approved by Prometheus.
    Standard,
    /// A private runtime bounded by a reviewed capability manifest.
    Bounded,
    /// A private runtime that bypasses the driver's own approvals: only
    /// while `/yolo` is on for the session that launched it.
    Unrestricted,
}

impl PermissionMode {
    /// The driver's name of the mode (`--permission-mode`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Bounded => "bounded",
            Self::Unrestricted => "unrestricted",
        }
    }

    /// Whether the runtime is private to one session.
    #[must_use]
    pub fn is_private(self) -> bool {
        self != Self::Standard
    }
}

/// What a runtime is launched with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSpec {
    /// Its permission mode.
    pub mode: PermissionMode,
    /// The reviewed capability manifest (`bounded`; optional otherwise).
    pub manifest: Option<PathBuf>,
    /// The session label sent with every call: it names the agent cursor
    /// overlay, so concurrent sessions and subagents show distinct ones.
    pub session_label: String,
    /// Show the agent cursor overlay (`--no-overlay` otherwise).
    pub overlay: bool,
}

/// One driver tool call's answer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DriverReply {
    /// The driver flagged the call as failed.
    pub is_error: bool,
    /// Its text content, joined.
    pub text: String,
    /// Its structured content (`Null` when absent).
    pub structured: Value,
    /// Its image parts: MIME type and decoded bytes.
    pub images: Vec<(String, Vec<u8>)>,
}

impl DriverReply {
    /// A field of the structured content.
    #[must_use]
    pub fn field(&self, key: &str) -> Option<&Value> {
        self.structured.get(key)
    }

    /// The reply's message: `message` of the structured content, else its
    /// text.
    #[must_use]
    pub fn message(&self) -> String {
        self.field("message")
            .and_then(Value::as_str)
            .map_or_else(|| self.text.clone(), str::to_owned)
    }
}

/// One live driver runtime.
pub trait Driver: Send + Sync {
    /// Calls the driver tool `tool` with `arguments`.
    fn call<'a>(
        &'a self,
        tool: &'a str,
        arguments: Map<String, Value>,
    ) -> SeamFuture<'a, Result<DriverReply, String>>;

    /// The tools the driver advertised.
    fn tools(&self) -> Vec<String>;
}

/// Starts driver runtimes for a backend.
pub trait Launcher: Send + Sync {
    /// Launches a runtime as `spec` says; `Err` says why it cannot run
    /// (the install hint, the identity or manifest failure...).
    fn launch<'a>(
        &'a self,
        spec: &'a RuntimeSpec,
    ) -> SeamFuture<'a, Result<Arc<dyn Driver>, String>>;
}
