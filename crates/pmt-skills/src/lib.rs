//! Skills: the agent's procedural memory (spec: skills).
//!
//! A skill is a directory holding an agentskills.io `SKILL.md` (YAML
//! frontmatter plus a Markdown body) and optional supporting files. This
//! crate finds skills across the search tiers ([`roots`], [`catalog`]),
//! decides which ones a session may see ([`gates`]), renders the frozen
//! prompt index and the per-turn change note ([`index`], [`session`]),
//! loads them ([`load`], [`inline`], [`secrets`]), serves the `skills`
//! toolset ([`tools`], [`manage`]) and keeps the profile-scoped records
//! (usage rows in `state.db`, [`lockfile`], [`pending`] writes).
//!
//! Wiring for a session (the session start owns it): build a
//! [`session::SkillsSession`] from the session's scope, attach it to
//! [`session::SkillSessions::process`] under the session id, set the
//! agent's prompt `skills_index` from [`session::SkillsSession::index_text`]
//! (and a `refresh_prompt` that re-reads it), add
//! [`session::SkillsSession::change_note`] to each turn's additions, give
//! the terminal [`session::SkillsSession::secret_source`], and register
//! [`tools::tools`] with the built-in tools.

pub mod activation;
pub mod bundled;
pub mod catalog;
pub mod digest;
pub mod eject;
pub mod frontmatter;
pub mod gates;
pub mod hints;
pub mod index;
pub mod inline;
pub mod layout;
pub mod lint;
pub mod load;
pub mod lockfile;
pub mod manage;
pub mod meta;
pub mod pending;
pub mod review;
pub mod roots;
pub mod scan;
pub mod secrets;
pub mod session;
pub mod slash;
pub mod source;
pub mod tools;
pub mod yaml;

pub use catalog::{Catalog, Skill};
pub use session::{SessionSetup, SkillSessions, SkillsSession};
