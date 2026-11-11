//! Projects in profile exports (spec: projects, Projects in backups and
//! exports). Whole-home backups and snapshots copy `state.db`, which holds
//! the projects; `profile export` leaves `state.db` out unless asked, so it
//! carries the projects as a JSON document that `profile import` writes
//! back. Folders are imported as recorded, never re-canonicalised or
//! dropped: a path that does not exist on the new machine shows as
//! `missing` ([`super::Folder::missing`]).

use pmt_core::error::Error;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::{Project, active_id, select, set_active};
use crate::error::sql;
use crate::store::Store;

/// The projects of a profile as an export carries them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectsExport {
    /// Every project, archived ones included.
    pub projects: Vec<Project>,
    /// The active project's id.
    #[serde(default)]
    pub active: Option<String>,
}

impl Store {
    /// Every project and the active pointer, for `profile export`.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn export_projects(&self) -> Result<ProjectsExport, Error> {
        self.read(|conn| {
            Ok(ProjectsExport {
                projects: select(conn, "?1 = ?1", "")?,
                active: active_id(conn)?,
            })
        })
    }

    /// Writes exported projects (same ids, folders as recorded). Projects
    /// whose id or slug the store already holds are skipped, so importing
    /// next to a restored `state.db` changes nothing. Returns how many
    /// were added.
    ///
    /// # Errors
    ///
    /// `store.*`.
    pub fn import_projects(&self, export: &ProjectsExport) -> Result<usize, Error> {
        self.write(|tx| {
            let mut added = 0;
            for project in &export.projects {
                let inserted = tx
                    .execute(
                        "INSERT INTO projects (id, slug, name, description, icon, color, board, \
                         created_at, archived) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
                         ON CONFLICT DO NOTHING",
                        params![
                            project.id.to_string(),
                            project.slug,
                            project.name,
                            project.description,
                            project.icon,
                            project.color,
                            project.board,
                            project.created_at.as_millisecond(),
                            project.archived
                        ],
                    )
                    .map_err(sql)?;
                if inserted == 0 {
                    continue;
                }
                for (n, folder) in project.folders.iter().enumerate() {
                    tx.execute(
                        "INSERT INTO project_folders (project_id, path, label, is_primary, added_at) \
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            project.id.to_string(),
                            folder.path,
                            folder.label,
                            folder.is_primary,
                            project.created_at.as_millisecond() + i64::try_from(n).unwrap_or(0)
                        ],
                    )
                    .map_err(sql)?;
                }
                added += 1;
            }
            if let Some(active) = &export.active
                && active_id(tx)?.is_none()
                && export.projects.iter().any(|p| p.id.to_string() == *active)
            {
                set_active(tx, Some(active))?;
            }
            Ok(added)
        })
    }
}
