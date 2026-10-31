//! Distribution and self-update (spec: distribution-and-update): the
//! release identity grammar ([`version`]), the embedded trust root and
//! signature checks ([`trust`]), release metadata ([`manifest`]), install
//! and update receipts ([`install`], [`receipts`]), the update lock
//! ([`lock`]), verified downloads ([`fetch`]) and the `prometheus update`
//! operations ([`flow`]): check, atomic swap after a self-test with
//! pre-update snapshots, and rollback.

pub mod archive;
pub mod fetch;
pub mod flow;
pub mod install;
pub mod lock;
pub mod manifest;
pub mod notice;
pub mod probe;
pub mod receipts;
pub mod swap;
pub mod trust;
pub mod version;

pub use fetch::{DEFAULT_ORIGIN, Identity, Releases};
pub use install::{InstallReceipt, Owner};
pub use trust::TrustRoot;
pub use version::{Channel, ReleaseVersion};
