//! Projects: per-profile, human-named workspaces spanning folders (spec:
//! projects, Project records; Active project and path ownership; Archive
//! and restore; Projects in backups and exports).
//!
//! A project is data in `state.db`, never ambient state: the active
//! project is a store pref, and the project of a session is resolved once,
//! from its `cwd`, when the session is created ([`owner_id`]). Folder
//! ownership is the longest folder of a non-archived project that is a
//! prefix of the path on component boundaries, so nested projects resolve
//! deterministically. Mutations live in [`edit`].

mod edit;
mod transfer;

use std::path::{Path, PathBuf};

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use pmt_core::id::ProjectId;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::sql;
use crate::store::Store;

pub use edit::NewProject;
pub use transfer::ProjectsExport;

/// The `store_meta` key of the active project's id (a store pref).
const ACTIVE_KEY: &str = "pref.active_project";

/// One folder of a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    /// Absolute, canonicalised when it was added.
    pub path: String,
    /// Optional label shown next to the path.
    #[serde(default)]
    pub label: Option<String>,
    /// The project's primary folder (at most one per project).
    #[serde(default)]
    pub is_primary: bool,
}

impl Folder {
    /// The folder no longer exists on this machine (an imported profile,
    /// a deleted checkout): kept and flagged instead of dropped.
    #[must_use]
    pub fn missing(&self) -> bool {
        !Path::new(&self.path).is_dir()
    }
}

/// A stored project with its folders (primary first, then by path).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    /// `prj_<ULID>`.
    pub id: ProjectId,
    /// Unique per profile, `^[a-z0-9][a-z0-9_-]{0,63}$`.
    pub slug: String,
    /// Display name.
    pub name: String,
    /// Optional description.
    #[serde(default)]
    pub description: Option<String>,
    /// Optional icon (an emoji or icon name).
    #[serde(default)]
    pub icon: Option<String>,
    /// Optional color.
    #[serde(default)]
    pub color: Option<String>,
    /// The bound kanban board's slug.
    #[serde(default)]
    pub board: Option<String>,
    /// Creation time (RFC 3339 in exports).
    #[serde(with = "rfc3339")]
    pub created_at: Timestamp,
    /// Hidden from default listings; its folders are released.
    #[serde(default)]
    pub archived: bool,
    /// Its folders.
    #[serde(default)]
    pub folders: Vec<Folder>,
}

impl Project {
    /// The primary folder's path.
    #[must_use]
    pub fn primary_path(&self) -> Option<&str> {
        self.folders
            .iter()
            .find(|f| f.is_primary)
            .map(|f| f.path.as_str())
    }
}

/// Timestamps as RFC 3339 text (jiff is built without its serde feature).
mod rfc3339 {
    use jiff::Timestamp;
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(at: &Timestamp, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(at)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Timestamp, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Checks `^[a-z0-9][a-z0-9_-]{0,63}$`.
///
/// # Errors
///
/// `project.invalid_slug`.
pub fn check_slug(slug: &str) -> Result<(), Error> {
    let mut chars = slug.chars();
    let valid = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && slug.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if valid {
        return Ok(());
    }
    Err(Error::new(
        &codes::PROJECT_INVALID_SLUG,
        format!(
            "invalid project slug '{slug}': use 1-64 of a-z, 0-9, '_' and '-', starting with a letter or digit"
        ),
    ))
}

/// The slug a name gives: lower case, every run of other characters one
/// `-`, trimmed and cut to 64 characters (`Prometheus Agent` ->
/// `prometheus-agent`); `None` when nothing is left.
#[must_use]
pub fn slug_of(name: &str) -> Option<String> {
    let mut slug = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.truncate(64);
    let slug = slug.trim_end_matches('-').to_owned();
    (!slug.is_empty()).then_some(slug)
}

/// `path` as a stored folder: absolute and canonical.
///
/// # Errors
///
/// `project.folder_invalid` when it is not an existing directory.
pub fn folder_path(path: &Path) -> Result<String, Error> {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let canonical = pmt_core::capability::canonical(&absolute);
    if !canonical.is_dir() {
        return Err(Error::new(
            &codes::PROJECT_FOLDER_INVALID,
            format!("folder {} does not exist", path.display()),
        ));
    }
    Ok(canonical.to_string_lossy().into_owned())
}

pub(crate) fn not_found(reference: &str) -> Error {
    Error::new(
        &codes::PROJECT_NOT_FOUND,
        format!("no project '{reference}' in this profile"),
    )
}

const COLUMNS: &str = "id, slug, name, description, icon, color, board, created_at, archived";

fn project_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(String, Project)> {
    let id: String = row.get(0)?;
    let parsed = ProjectId::parse(&id).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok((
        id,
        Project {
            id: parsed,
            slug: row.get(1)?,
            name: row.get(2)?,
            description: row.get(3)?,
            icon: row.get(4)?,
            color: row.get(5)?,
            board: row.get(6)?,
            created_at: Timestamp::from_millisecond(row.get(7)?).unwrap_or(Timestamp::UNIX_EPOCH),
            archived: row.get(8)?,
            folders: Vec::new(),
        },
    ))
}

fn folders_of(conn: &Connection, id: &str) -> Result<Vec<Folder>, Error> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT path, label, is_primary FROM project_folders WHERE project_id = ?1 \
             ORDER BY is_primary DESC, path",
        )
        .map_err(sql)?;
    let rows = stmt
        .query_map([id], |row| {
            Ok(Folder {
                path: row.get(0)?,
                label: row.get(1)?,
                is_primary: row.get(2)?,
            })
        })
        .map_err(sql)?;
    rows.collect::<Result<_, _>>().map_err(sql)
}

fn select(conn: &Connection, filter: &str, param: &str) -> Result<Vec<Project>, Error> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {COLUMNS} FROM projects WHERE {filter} ORDER BY archived, slug"
        ))
        .map_err(sql)?;
    let rows = stmt
        .query_map(params![param], project_from_row)
        .map_err(sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql)?;
    rows.into_iter()
        .map(|(id, mut project)| {
            project.folders = folders_of(conn, &id)?;
            Ok(project)
        })
        .collect()
}

/// The project whose id or slug is `reference`.
pub(crate) fn find(conn: &Connection, reference: &str) -> Result<Option<Project>, Error> {
    Ok(select(conn, "id = ?1 OR slug = ?1", reference)?.pop())
}

/// [`find`], failing with `project.not_found`.
pub(crate) fn resolve(conn: &Connection, reference: &str) -> Result<Project, Error> {
    find(conn, reference)?.ok_or_else(|| not_found(reference))
}

pub(crate) fn active_id(conn: &Connection) -> Result<Option<String>, Error> {
    conn.query_row(
        "SELECT value FROM store_meta WHERE key = ?1",
        [ACTIVE_KEY],
        |row| row.get(0),
    )
    .optional()
    .map_err(sql)
}

/// Points the active project at `id` (`None` clears it).
pub(crate) fn set_active(tx: &rusqlite::Transaction<'_>, id: Option<&str>) -> Result<(), Error> {
    match id {
        Some(id) => tx.execute(
            "INSERT INTO store_meta (key, value) VALUES (?1, ?2) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![ACTIVE_KEY, id],
        ),
        None => tx.execute("DELETE FROM store_meta WHERE key = ?1", [ACTIVE_KEY]),
    }
    .map(drop)
    .map_err(sql)
}

/// Every folder of a non-archived project with its owner: `(path,
/// project id, slug)`.
pub(crate) fn live_folders(conn: &Connection) -> Result<Vec<(String, String, String)>, Error> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT f.path, p.id, p.slug FROM project_folders f \
             JOIN projects p ON p.id = f.project_id WHERE p.archived = 0",
        )
        .map_err(sql)?;
    let rows = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .map_err(sql)?;
    rows.collect::<Result<_, _>>().map_err(sql)
}

/// The id of the non-archived project owning `path`: the one with the
/// longest folder that is a prefix of it on component boundaries.
pub(crate) fn owner_id(conn: &Connection, path: &Path) -> Result<Option<String>, Error> {
    let owner = live_folders(conn)?
        .into_iter()
        .filter(|(folder, _, _)| path.starts_with(folder))
        .max_by_key(|(folder, _, _)| Path::new(folder).components().count())
        .map(|(_, id, _)| id);
    Ok(owner)
}

/// `cwd` as ownership compares it: absolute and canonical where it exists.
pub(crate) fn lookup_path(cwd: &str) -> PathBuf {
    pmt_core::capability::canonical(Path::new(cwd))
}

impl Store {
    /// The profile's projects by slug; archived ones only with
    /// `include_archived` (listed after the live ones).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn projects(&self, include_archived: bool) -> Result<Vec<Project>, Error> {
        let filter = if include_archived {
            "?1 = ?1"
        } else {
            "archived = 0 AND ?1 = ?1"
        };
        self.read(|conn| select(conn, filter, ""))
    }

    /// The project with id or slug `reference`.
    ///
    /// # Errors
    ///
    /// `project.not_found` (66), `store.sqlite`.
    pub fn project(&self, reference: &str) -> Result<Project, Error> {
        self.read(|conn| resolve(conn, reference))
    }

    /// The active project, if one is set.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn active_project(&self) -> Result<Option<Project>, Error> {
        self.read(|conn| match active_id(conn)? {
            Some(id) => find(conn, &id),
            None => Ok(None),
        })
    }

    /// The non-archived project owning `path` (nested folders resolve to
    /// the innermost project).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn project_owning(&self, path: &Path) -> Result<Option<Project>, Error> {
        let path = lookup_path(&path.to_string_lossy());
        self.read(|conn| match owner_id(conn, &path)? {
            Some(id) => find(conn, &id),
            None => Ok(None),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{check_slug, slug_of};

    #[test]
    fn names_give_slugs() {
        assert_eq!(
            slug_of("Prometheus Agent").as_deref(),
            Some("prometheus-agent")
        );
        assert_eq!(slug_of("  API / v2!! ").as_deref(), Some("api-v2"));
        assert_eq!(slug_of("日本"), None);
        let long = "a".repeat(80);
        assert_eq!(slug_of(&long).unwrap().len(), 64);
    }

    #[test]
    fn slugs_follow_the_pattern() {
        assert!(check_slug("web_2-x").is_ok());
        assert!(check_slug("-web").is_err());
        assert!(check_slug("Web").is_err());
        assert!(check_slug(&"a".repeat(65)).is_err());
    }
}
