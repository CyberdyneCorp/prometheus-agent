//! The learning journey's nodes (spec: memory, Learning journey
//! timeline): learned skills and memory entries, oldest first. A skill is
//! learned when the user (`/learn`) or the agent (including a background
//! review) created it, or when it was used at least once; bundled and hub
//! skills nobody used stay out. Memory entries are the current entries the
//! revision journal records, with the origin that wrote them.

use std::path::Path;
use std::time::SystemTime;

use jiff::Timestamp;
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_rpc::contracts::{JourneyList, JourneyNode};
use pmt_skills::bundled::Bundled;
use pmt_skills::eject;
use pmt_skills::layout::SKILL_FILE;
use pmt_store::{CreatedBy, MemoryEntry, SkillState, SkillUsage, Store};

/// The prefix of a skill node's id.
pub const SKILL_PREFIX: &str = "skill:";

/// The longest node title, in characters.
const TITLE_CHARS: usize = 72;

/// The journey of `scope`: nodes created at or after `since`, oldest
/// first.
///
/// # Errors
///
/// `cli.usage` for a `since` that does not read; store errors.
pub fn list(
    scope: &ProfileScope,
    store: &Store,
    user_home: Option<&Path>,
    since: Option<&str>,
) -> Result<JourneyList, Error> {
    let since = since
        .map(|s| parse_since(s, Timestamp::now()))
        .transpose()?;
    let mut nodes = skill_nodes(scope, store, user_home)?;
    nodes.extend(store.memory_entries(None)?.iter().map(memory_node));
    if let Some(since) = since {
        nodes.retain(|n| {
            n.created_at
                .parse::<Timestamp>()
                .is_ok_and(|at| at >= since)
        });
    }
    nodes.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
    Ok(JourneyList { nodes })
}

/// Reads `--since`: a duration back from `now` (`45m`, `2h`, `3d`,
/// `1w`), a date (`2026-11-01`, midnight UTC) or a timestamp.
///
/// # Errors
///
/// `cli.usage` for anything else.
pub fn parse_since(text: &str, now: Timestamp) -> Result<Timestamp, Error> {
    let text = text.trim();
    if let Ok(at) = text.parse::<Timestamp>() {
        return Ok(at);
    }
    if let Ok(date) = text.parse::<jiff::civil::Date>()
        && let Ok(at) = date.to_zoned(jiff::tz::TimeZone::UTC)
    {
        return Ok(at.timestamp());
    }
    match pmt_memory::journal::Since::parse(text, now) {
        Ok(pmt_memory::journal::Since::Time(at)) => Ok(at),
        _ => Err(Error::new(
            &codes::CLI_USAGE,
            format!("--since expects a duration like 2h, 3d or 1w, or a date, not '{text}'"),
        )),
    }
}

/// Whether a skill with `usage` belongs on the journey.
fn learned(usage: &SkillUsage) -> bool {
    usage.state != SkillState::Archived
        && (matches!(usage.created_by, CreatedBy::User | CreatedBy::Agent) || usage.use_count > 0)
}

fn skill_nodes(
    scope: &ProfileScope,
    store: &Store,
    user_home: Option<&Path>,
) -> Result<Vec<JourneyNode>, Error> {
    let catalog = catalog(scope, user_home);
    let nodes = store
        .skill_usages()?
        .into_iter()
        .filter(learned)
        .filter_map(|usage| {
            let skill = catalog.lookup(&usage.name).ok()?;
            Some(skill_node(&usage, skill.dir.path()))
        })
        .collect();
    Ok(nodes)
}

fn skill_node(usage: &SkillUsage, dir: Option<&Path>) -> JourneyNode {
    let file = dir.and_then(|d| std::fs::metadata(d.join(SKILL_FILE)).ok());
    let stamp = |time: std::io::Result<SystemTime>| {
        time.ok()
            .and_then(|t| Timestamp::try_from(t).ok())
            .or(usage.last_activity_at)
            .unwrap_or_else(Timestamp::now)
    };
    let created = stamp(file.as_ref().map_or_else(
        || Err(std::io::ErrorKind::NotFound.into()),
        |m| m.created().or_else(|_| m.modified()),
    ));
    let updated = stamp(file.as_ref().map_or_else(
        || Err(std::io::ErrorKind::NotFound.into()),
        std::fs::Metadata::modified,
    ));
    JourneyNode {
        id: format!("{SKILL_PREFIX}{}", usage.name),
        kind: "skill".to_owned(),
        title: usage.name.clone(),
        created_at: created.to_string(),
        updated_at: updated.max(created).to_string(),
        origin: usage.created_by.as_str().to_owned(),
    }
}

fn memory_node(entry: &MemoryEntry) -> JourneyNode {
    JourneyNode {
        id: entry.id.clone(),
        kind: "memory".to_owned(),
        title: title(&entry.content),
        created_at: entry.created_at.to_string(),
        updated_at: entry.updated_at.to_string(),
        origin: entry.origin.as_str().to_owned(),
    }
}

/// `text` on one line, cut to [`TITLE_CHARS`].
fn title(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= TITLE_CHARS {
        return line;
    }
    let cut: String = line.chars().take(TITLE_CHARS - 1).collect();
    format!("{cut}…")
}

/// A node named on the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    /// `skill:<name>`.
    Skill(String),
    /// A memory entry id.
    Memory(String),
}

impl Node {
    /// Reads a node id: `skill:<name>` or an entry id (`m_...`).
    ///
    /// # Errors
    ///
    /// `cli.usage` for anything else.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let text = text.trim();
        if let Some(name) = text.strip_prefix(SKILL_PREFIX).filter(|n| !n.is_empty()) {
            return Ok(Self::Skill(name.to_owned()));
        }
        if text.starts_with("m_") {
            return Ok(Self::Memory(text.to_owned()));
        }
        Err(Error::new(
            &codes::CLI_USAGE,
            format!("'{text}' is no journey node: use skill:<name> or a memory id (m_...)"),
        )
        .with_hint("prometheus journey list"))
    }
}

/// The profile's skills seen without a project (the journey's view).
#[must_use]
pub fn catalog(scope: &ProfileScope, user_home: Option<&Path>) -> pmt_skills::Catalog {
    eject::profile_catalog(scope, None, user_home, Bundled::builtin())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_reads_durations_dates_and_timestamps_but_not_revisions() {
        let now: Timestamp = "2026-11-08T12:00:00Z".parse().unwrap();
        assert_eq!(
            parse_since("2h", now).unwrap().to_string(),
            "2026-11-08T10:00:00Z"
        );
        assert_eq!(
            parse_since("2026-11-01", now).unwrap().to_string(),
            "2026-11-01T00:00:00Z"
        );
        assert_eq!(
            parse_since("2026-11-07T08:30:00Z", now)
                .unwrap()
                .to_string(),
            "2026-11-07T08:30:00Z"
        );
        assert!(parse_since("12", now).is_err());
        assert!(parse_since("soon", now).is_err());
    }

    #[test]
    fn node_ids_name_a_skill_or_an_entry() {
        assert_eq!(
            Node::parse("skill:deploy-checklist").unwrap(),
            Node::Skill("deploy-checklist".to_owned())
        );
        assert_eq!(
            Node::parse("m_abcdefghijkl").unwrap(),
            Node::Memory("m_abcdefghijkl".to_owned())
        );
        assert!(Node::parse("skill:").is_err());
        assert!(Node::parse("deploy").is_err());
    }
}
