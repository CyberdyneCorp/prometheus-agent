//! What may be in a harness and how one is checked: the editable surface,
//! validation, and the rules a profile supplies. The tree type and its
//! on-disk versions are `pmt_config::harness`.

pub mod diff;
pub mod surface;
pub mod validate;

pub use validate::hooks::{HarnessHook, hooks_of};
pub use validate::{Rules, Validated, validate};
