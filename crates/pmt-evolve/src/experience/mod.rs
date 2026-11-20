//! The experience library (spec: experience-library): what harness
//! evolution learned in one task, carried to later work in the same
//! domain. After a mode's selection a reflector distils at most one
//! lesson ([`lesson`]); when the proposal is adopted, the adopted tree and
//! its `EXPERIENCE.md` become an entry of `<profile>/experience/<domain>/`
//! ([`record`], [`entry`], [`library`]). Later jobs of the domain see a
//! snapshot of the library in their evidence, and (opt-in) later sessions
//! of the domain see the lessons as a frozen prompt layer ([`ops`]).
//! Domains are normalised names ([`domain`]); a domain moves between
//! profiles as an archive ([`archive`]). Nothing derived from a grader,
//! rubric or eval verdict may enter an entry.

pub mod archive;
pub mod domain;
pub mod entry;
pub mod lesson;
pub mod library;
pub mod log;
pub mod ops;
pub mod record;

pub use library::Library;
pub use record::{Outcome, Settings, adopt};
