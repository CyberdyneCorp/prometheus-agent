//! Project mutations: create, folders, rename, primary, activation,
//! archive and restore, board binding. Every check (folder ownership, slug
//! uniqueness) runs inside the write transaction that applies the change,
//! so two writers cannot both claim one folder.

use std::path::Path;

use pmt_core::error::{Error, codes};
use pmt_core::id::ProjectId;
use rusqlite::{Transaction, params};

use super::{
    ACTIVE_KEY, Project, check_slug, folder_path, live_folders, resolve, set_active, slug_of,
};
use crate::error::sql;
use crate::sessions::now_ms;
use crate::store::Store;

/// A project to create.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewProject {
    /// Display name.
    pub name: String,
    /// Explicit slug (default: derived from the name, numbered when taken).
    pub slug: Option<String>,
    /// Folders; the first is primary unless `primary` names another.
    pub folders: Vec<std::path::PathBuf>,
    /// The primary folder (added when not among `folders`).
    pub primary: Option<std::path::PathBuf>,
    /// Optional description.
    pub description: Option<String>,
    /// Optional icon.
    pub icon: Option<String>,
    /// Optional color.
    pub color: Option<String>,
    /// Bound kanban board slug.
    pub board: Option<String>,
}

fn claimed(path: &str, owner: &str, now: bool) -> Error {
    let verb = if now {
        "now belongs"
    } else {
        "already belongs"
    };
    Error::new(
        &codes::PROJECT_FOLDER_CLAIMED,
        format!("folder {path} {verb} to project '{owner}'"),
    )
}

/// Fails when a live project other than `project` owns `path`.
fn ensure_unclaimed(
    tx: &Transaction<'_>,
    path: &str,
    project: &str,
    now: bool,
) -> Result<(), Error> {
    match live_folders(tx)?
        .into_iter()
        .find(|(folder, id, _)| folder == path && id != project)
    {
        Some((_, _, slug)) => Err(claimed(path, &slug, now)),
        None => Ok(()),
    }
}

fn ensure_live(project: &Project) -> Result<(), Error> {
    if project.archived {
        return Err(Error::new(
            &codes::PROJECT_ARCHIVED,
            format!("project '{}' is archived", project.slug),
        ));
    }
    Ok(())
}

fn slug_taken(tx: &Transaction<'_>, slug: &str) -> Result<bool, Error> {
    tx.query_row(
        "SELECT COUNT(*) FROM projects WHERE slug = ?1",
        [slug],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .map_err(sql)
}

/// The slug of a new project: the explicit one (which must be free), else
/// the name's, numbered `-2`, `-3`... while taken.
fn new_slug(tx: &Transaction<'_>, new: &NewProject) -> Result<String, Error> {
    if let Some(slug) = &new.slug {
        check_slug(slug)?;
        if slug_taken(tx, slug)? {
            return Err(Error::new(
                &codes::PROJECT_SLUG_IN_USE,
                format!("project slug '{slug}' is already in use"),
            ));
        }
        return Ok(slug.clone());
    }
    let base = slug_of(&new.name).ok_or_else(|| {
        Error::new(
            &codes::PROJECT_INVALID_SLUG,
            format!("project name '{}' gives no slug; pass --slug", new.name),
        )
    })?;
    let mut slug = base.clone();
    let mut n = 2;
    while slug_taken(tx, &slug)? {
        let suffix = format!("-{n}");
        let mut stem = base.clone();
        stem.truncate(64 - suffix.len());
        slug = format!("{}{suffix}", stem.trim_end_matches('-'));
        n += 1;
    }
    Ok(slug)
}

fn insert_folder(
    tx: &Transaction<'_>,
    project: &str,
    path: &str,
    label: Option<&str>,
    primary: bool,
) -> Result<(), Error> {
    ensure_unclaimed(tx, path, project, false)?;
    if primary {
        tx.execute(
            "UPDATE project_folders SET is_primary = 0 WHERE project_id = ?1",
            [project],
        )
        .map_err(sql)?;
    }
    tx.execute(
        "INSERT INTO project_folders (project_id, path, label, is_primary, added_at) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT (project_id, path) DO UPDATE SET \
         label = COALESCE(excluded.label, label), is_primary = excluded.is_primary OR is_primary",
        params![project, path, label, primary, now_ms()],
    )
    .map_err(sql)?;
    Ok(())
}

/// The project's folders, primary first: `(path, primary)`.
fn ordered_folders(new: &NewProject) -> Result<Vec<(String, bool)>, Error> {
    let primary = new.primary.as_deref().map(folder_path).transpose()?;
    let mut folders: Vec<(String, bool)> = Vec::new();
    if let Some(primary) = &primary {
        folders.push((primary.clone(), true));
    }
    for path in &new.folders {
        let path = folder_path(path)?;
        if !folders.iter().any(|(p, _)| *p == path) {
            let is_primary = folders.is_empty();
            folders.push((path, is_primary));
        }
    }
    Ok(folders)
}

impl Store {
    /// Creates a project (and, with `activate`, makes it the active one).
    ///
    /// # Errors
    ///
    /// `project.invalid_slug`, `project.slug_in_use`,
    /// `project.folder_invalid`, `project.folder_claimed`, `store.*`.
    pub fn create_project(&self, new: &NewProject, activate: bool) -> Result<Project, Error> {
        let folders = ordered_folders(new)?;
        self.write(|tx| {
            let id = ProjectId::new().to_string();
            let slug = new_slug(tx, new)?;
            tx.execute(
                "INSERT INTO projects (id, slug, name, description, icon, color, board, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    id,
                    slug,
                    new.name,
                    new.description,
                    new.icon,
                    new.color,
                    new.board,
                    now_ms()
                ],
            )
            .map_err(sql)?;
            for (path, primary) in &folders {
                insert_folder(tx, &id, path, None, *primary)?;
            }
            if activate {
                set_active(tx, Some(&id))?;
            }
            resolve(tx, &id)
        })
    }

    /// Adds `path` to a live project (as primary with `primary`, or when
    /// the project has none yet).
    ///
    /// # Errors
    ///
    /// `project.not_found`, `project.archived`, `project.folder_invalid`,
    /// `project.folder_claimed`, `store.*`.
    pub fn add_project_folder(
        &self,
        reference: &str,
        path: &Path,
        label: Option<&str>,
        primary: bool,
    ) -> Result<Project, Error> {
        let path = folder_path(path)?;
        self.write(|tx| {
            let project = resolve(tx, reference)?;
            ensure_live(&project)?;
            let id = project.id.to_string();
            let primary = primary || project.primary_path().is_none();
            insert_folder(tx, &id, &path, label, primary)?;
            resolve(tx, &id)
        })
    }

    /// Removes a folder from a project; removing the primary promotes the
    /// remaining folder added first.
    ///
    /// # Errors
    ///
    /// `project.not_found`, `project.folder_invalid` (not one of its
    /// folders), `store.*`.
    pub fn remove_project_folder(&self, reference: &str, path: &Path) -> Result<Project, Error> {
        self.write(|tx| {
            let project = resolve(tx, reference)?;
            let folder = member(&project, path)?;
            let id = project.id.to_string();
            tx.execute(
                "DELETE FROM project_folders WHERE project_id = ?1 AND path = ?2",
                params![id, folder],
            )
            .map_err(sql)?;
            tx.execute(
                "UPDATE project_folders SET is_primary = 1 WHERE project_id = ?1 \
                 AND NOT EXISTS (SELECT 1 FROM project_folders WHERE project_id = ?1 AND is_primary = 1) \
                 AND path = (SELECT path FROM project_folders WHERE project_id = ?1 \
                             ORDER BY added_at, path LIMIT 1)",
                [&id],
            )
            .map_err(sql)?;
            resolve(tx, &id)
        })
    }

    /// Makes `path`, already one of the project's folders, its primary.
    ///
    /// # Errors
    ///
    /// `project.not_found`, `project.folder_invalid`, `store.*`.
    pub fn set_project_primary(&self, reference: &str, path: &Path) -> Result<Project, Error> {
        self.write(|tx| {
            let project = resolve(tx, reference)?;
            let folder = member(&project, path)?;
            let id = project.id.to_string();
            insert_folder(tx, &id, &folder, None, true)?;
            resolve(tx, &id)
        })
    }

    /// Renames a project (its slug stays).
    ///
    /// # Errors
    ///
    /// `project.not_found`, `store.*`.
    pub fn rename_project(&self, reference: &str, name: &str) -> Result<Project, Error> {
        self.update_project(reference, "name", Some(name))
    }

    /// Binds a project to kanban board `board` (`None` unbinds).
    ///
    /// # Errors
    ///
    /// `project.not_found`, `store.*`.
    pub fn bind_project_board(
        &self,
        reference: &str,
        board: Option<&str>,
    ) -> Result<Project, Error> {
        self.update_project(reference, "board", board)
    }

    fn update_project(
        &self,
        reference: &str,
        column: &'static str,
        value: Option<&str>,
    ) -> Result<Project, Error> {
        self.write(|tx| {
            let id = resolve(tx, reference)?.id.to_string();
            tx.execute(
                &format!("UPDATE projects SET {column} = ?2 WHERE id = ?1"),
                params![id, value],
            )
            .map_err(sql)?;
            resolve(tx, &id)
        })
    }

    /// Makes `reference` the active project (`None` clears it).
    ///
    /// # Errors
    ///
    /// `project.not_found`, `project.archived`, `store.*`.
    pub fn use_project(&self, reference: Option<&str>) -> Result<Option<Project>, Error> {
        self.write(|tx| {
            let Some(reference) = reference else {
                set_active(tx, None)?;
                return Ok(None);
            };
            let project = resolve(tx, reference)?;
            ensure_live(&project)?;
            set_active(tx, Some(&project.id.to_string()))?;
            Ok(Some(project))
        })
    }

    /// Archives a project: hidden from default listings, its folders
    /// released, sessions and folder rows kept; the active pointer is
    /// cleared when it pointed here.
    ///
    /// # Errors
    ///
    /// `project.not_found`, `store.*`.
    pub fn archive_project(&self, reference: &str) -> Result<Project, Error> {
        self.write(|tx| {
            let id = resolve(tx, reference)?.id.to_string();
            tx.execute("UPDATE projects SET archived = 1 WHERE id = ?1", [&id])
                .map_err(sql)?;
            tx.execute(
                "DELETE FROM store_meta WHERE key = ?1 AND value = ?2",
                params![ACTIVE_KEY, id],
            )
            .map_err(sql)?;
            resolve(tx, &id)
        })
    }

    /// Restores an archived project, failing when one of its folders was
    /// claimed by another live project meanwhile.
    ///
    /// # Errors
    ///
    /// `project.not_found`, `project.folder_claimed`, `store.*`.
    pub fn restore_project(&self, reference: &str) -> Result<Project, Error> {
        self.write(|tx| {
            let project = resolve(tx, reference)?;
            let id = project.id.to_string();
            for folder in &project.folders {
                ensure_unclaimed(tx, &folder.path, &id, true)?;
            }
            tx.execute("UPDATE projects SET archived = 0 WHERE id = ?1", [&id])
                .map_err(sql)?;
            resolve(tx, &id)
        })
    }
}

/// The stored folder of `project` that `path` names (as given, or
/// canonicalised when it still exists).
fn member(project: &Project, path: &Path) -> Result<String, Error> {
    let given = path.to_string_lossy();
    let canonical = folder_path(path).ok();
    project
        .folders
        .iter()
        .find(|f| f.path == given || Some(&f.path) == canonical.as_ref())
        .map(|f| f.path.clone())
        .ok_or_else(|| {
            Error::new(
                &codes::PROJECT_FOLDER_INVALID,
                format!(
                    "{} is not a folder of project '{}'",
                    path.display(),
                    project.slug
                ),
            )
        })
}
