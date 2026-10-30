//! OS execution sandboxing derived from a child's effective capability set
//! (design D8; spec: security-boundaries, execution-environments).
//!
//! - [`Policy`] translates capabilities into what the OS can enforce, with
//!   credential stores denied by default.
//! - [`Sandbox`] probes the host once per session, reports enforcement per
//!   feature ([`StrengthReport`]), applies the `security.sandbox.allow_degraded`
//!   rule (refuse, or WARN on every start) and builds confined
//!   [`std::process::Command`]s: Landlock + seccomp on Linux, a Seatbelt
//!   profile on macOS, an `AppContainer` in a Job Object on Windows.
//! - [`backend`] is the typed terminal backend registry and readiness check.
//!
//! On Windows a confined child is started by a small launcher process (the
//! standard library cannot create `AppContainer` processes); executables
//! that spawn sandboxed children must call [`launcher::run_if_requested`]
//! first thing in `main`.

pub mod backend;
pub mod launcher;
mod locations;
pub mod policy;
pub mod report;
mod sandbox;

#[cfg(target_os = "linux")]
#[path = "linux/mod.rs"]
mod platform;

#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod platform;

#[cfg(windows)]
#[path = "windows/mod.rs"]
mod platform;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
#[path = "other.rs"]
mod platform;

pub use launcher::LAUNCH_ARG;
pub use policy::{BASELINE, Paths, Policy, Ports};
pub use report::{Enforcement, Feature, FeatureReport, Gap, Level, StrengthReport};
pub use sandbox::{Options, Sandbox};
