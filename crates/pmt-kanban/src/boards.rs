//! Board storage and selection (spec: kanban, Board storage location;
//! Board slugs and selection). Boards live at the home root, under
//! `<home>/kanban/boards/<slug>/` (`board.db`, `board.toml`, `workspaces/`,
//! `logs/`), never inside a profile and never at a path the environment
//! can redirect; attachments share the home's content-addressed blob store
//! `<home>/kanban/blobs/`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use serde::{Deserialize, Serialize};

use crate::board::Board;
use crate::contract::PrChecks;
use crate::observe::Observer;

/// The board every home has.
pub const DEFAULT_BOARD: &str = "default";

/// `<home>/kanban`: the boards of one home. Boards it opens carry its
/// pull-request checker and lifecycle observer, when it has them.
#[derive(Clone)]
pub struct Boards {
    root: PathBuf,
    pr_checks: Option<Arc<dyn PrChecks>>,
    observer: Option<Arc<dyn Observer>>,
}

impl std::fmt::Debug for Boards {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Boards")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

/// `board.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BoardSettings {
    /// The display name.
    pub name: String,
    /// Worker completions go to `review` unless a task says otherwise.
    pub review: bool,
    /// Archived boards cannot be selected.
    pub archived: bool,
    /// Creation instant (Unix ms).
    pub created_at: i64,
}

/// One row of `boards list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BoardInfo {
    /// Its slug.
    pub slug: String,
    /// `board.toml`.
    #[serde(flatten)]
    pub settings: BoardSettings,
}

/// Checks and normalises a board slug.
///
/// # Errors
///
/// `kanban.invalid_slug`.
pub fn slug(text: &str) -> Result<String, Error> {
    let lower = text.to_lowercase();
    let mut chars = lower.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest_ok =
        chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if first_ok && rest_ok && lower.len() <= 64 {
        Ok(lower)
    } else {
        Err(Error::new(
            &codes::KANBAN_INVALID_SLUG,
            format!(
                "invalid board slug '{text}': 1-64 chars, lowercase alphanumerics, '-' or '_', not starting with '-' or '_'"
            ),
        ))
    }
}

fn not_found(slug: &str) -> Error {
    Error::new(
        &codes::KANBAN_BOARD_NOT_FOUND,
        format!("board '{slug}' not found"),
    )
}

impl Boards {
    /// The boards of the home at `home`; creates the `default` board on
    /// first use.
    ///
    /// # Errors
    ///
    /// `home.io`, store errors.
    pub fn open(home: &Path) -> Result<Self, Error> {
        let boards = Self {
            root: home.join("kanban"),
            pr_checks: None,
            observer: None,
        };
        for dir in [boards.root.clone(), boards.boards_dir(), boards.blobs_dir()] {
            private_fs::create_dir(&dir).map_err(|e| Error::io(&codes::HOME_IO, &dir, &e))?;
        }
        if !boards.dir(DEFAULT_BOARD).join("board.toml").exists() {
            boards.make(DEFAULT_BOARD, "Default")?;
        }
        Ok(boards)
    }

    /// `<home>/kanban`.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn boards_dir(&self) -> PathBuf {
        self.root.join("boards")
    }

    /// The shared attachment blob store.
    #[must_use]
    pub fn blobs_dir(&self) -> PathBuf {
        self.root.join("blobs")
    }

    /// `<home>/kanban/boards/<slug>`.
    #[must_use]
    pub fn dir(&self, slug: &str) -> PathBuf {
        self.boards_dir().join(slug)
    }

    /// Every board, archived ones included, by slug.
    ///
    /// # Errors
    ///
    /// `home.io`, a malformed `board.toml`.
    pub fn list(&self) -> Result<Vec<BoardInfo>, Error> {
        let dir = self.boards_dir();
        let entries = fs::read_dir(&dir).map_err(|e| Error::io(&codes::HOME_IO, &dir, &e))?;
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if slug(&name).ok().as_deref() == Some(name.as_str())
                && entry.path().join("board.toml").is_file()
            {
                out.push(BoardInfo {
                    settings: self.settings(&name)?,
                    slug: name,
                });
            }
        }
        out.sort_by(|a, b| a.slug.cmp(&b.slug));
        Ok(out)
    }

    /// A board's `board.toml`.
    ///
    /// # Errors
    ///
    /// `kanban.board_not_found`, a malformed file.
    pub fn settings(&self, slug: &str) -> Result<BoardSettings, Error> {
        let path = self.dir(slug).join("board.toml");
        let text = fs::read_to_string(&path).map_err(|_| not_found(slug))?;
        toml::from_str(&text).map_err(|e| {
            Error::new(
                &codes::CONFIG_PARSE,
                format!("{}: {}", path.display(), e.message()),
            )
        })
    }

    /// Writes a board's `board.toml`.
    ///
    /// # Errors
    ///
    /// `home.io`.
    pub fn save_settings(&self, slug: &str, settings: &BoardSettings) -> Result<(), Error> {
        let path = self.dir(slug).join("board.toml");
        let text = toml::to_string(settings).unwrap_or_default();
        private_fs::write_atomic(&path, text.as_bytes())
            .map_err(|e| Error::io(&codes::HOME_IO, &path, &e))
    }

    /// The board a command works on: `flag`, else `current` (the invoking
    /// profile's `kanban.current_board`), else `default`.
    ///
    /// # Errors
    ///
    /// `kanban.invalid_slug`; `kanban.board_not_found` for a missing or
    /// archived board, with a hint when the selection came from config.
    pub fn resolve(&self, flag: Option<&str>, current: Option<&str>) -> Result<String, Error> {
        let (wanted, from_config) = match (flag, current) {
            (Some(flag), _) => (slug(flag)?, false),
            (None, Some(current)) => (slug(current)?, true),
            (None, None) => (DEFAULT_BOARD.to_owned(), false),
        };
        match self.settings(&wanted) {
            Ok(settings) if !settings.archived => Ok(wanted),
            _ if from_config => Err(Error::new(
                &codes::KANBAN_BOARD_NOT_FOUND,
                format!("board '{wanted}' not found (set kanban.current_board or pass --board)"),
            )),
            _ => Err(not_found(&wanted)),
        }
    }

    /// The boards of `home` as they are, creating nothing (an external
    /// worker's sandbox reaches only its own board and the blob store).
    #[must_use]
    pub fn existing(home: &Path) -> Self {
        Self {
            root: home.join("kanban"),
            pr_checks: None,
            observer: None,
        }
    }

    /// Boards opened from here read pull-request evidence with `checks`.
    #[must_use]
    pub fn with_pr_checks(mut self, checks: Arc<dyn PrChecks>) -> Self {
        self.pr_checks = Some(checks);
        self
    }

    /// Boards opened from here hand their committed events to `observer`.
    #[must_use]
    pub fn with_observer(mut self, observer: Arc<dyn Observer>) -> Self {
        self.observer = Some(observer);
        self
    }

    fn equip(&self, mut board: Board) -> Board {
        if let Some(checks) = &self.pr_checks {
            board = board.with_pr_checks(Arc::clone(checks));
        }
        if let Some(observer) = &self.observer {
            board = board.with_observer(Arc::clone(observer));
        }
        board
    }

    /// Opens a live board.
    ///
    /// # Errors
    ///
    /// `kanban.board_not_found`, store errors.
    pub fn board(&self, slug: &str) -> Result<Board, Error> {
        let settings = self.settings(slug)?;
        if settings.archived {
            return Err(not_found(slug));
        }
        Board::open(self, slug, settings).map(|b| self.equip(b))
    }

    /// Opens a board whether or not it is archived (export, gc).
    ///
    /// # Errors
    ///
    /// `kanban.board_not_found`, store errors.
    pub fn board_any(&self, slug: &str) -> Result<Board, Error> {
        let settings = self.settings(slug)?;
        Board::open(self, slug, settings).map(|b| self.equip(b))
    }

    /// Creates a board.
    ///
    /// # Errors
    ///
    /// `kanban.invalid_slug`, `kanban.board_exists`, `home.io`.
    pub fn create(&self, wanted: &str, name: Option<&str>) -> Result<String, Error> {
        let slug = slug(wanted)?;
        if self.dir(&slug).join("board.toml").exists() {
            return Err(Error::new(
                &codes::KANBAN_BOARD_EXISTS,
                format!("board '{slug}' already exists"),
            ));
        }
        self.make(&slug, name.unwrap_or(&slug))?;
        Ok(slug)
    }

    fn make(&self, slug: &str, name: &str) -> Result<(), Error> {
        let dir = self.dir(slug);
        for sub in [dir.clone(), dir.join("workspaces"), dir.join("logs")] {
            private_fs::create_dir(&sub).map_err(|e| Error::io(&codes::HOME_IO, &sub, &e))?;
        }
        let settings = BoardSettings {
            name: name.to_owned(),
            created_at: crate::model::now_ms(),
            ..BoardSettings::default()
        };
        // The database first: a board is listed once its board.toml exists.
        drop(Board::open(self, slug, settings.clone())?);
        self.save_settings(slug, &settings)
    }

    /// Renames a board's directory.
    ///
    /// # Errors
    ///
    /// Slug errors, `kanban.board_not_found`, `kanban.board_exists`,
    /// `home.io`. The default board cannot be renamed.
    pub fn rename(&self, from: &str, to: &str) -> Result<String, Error> {
        let from = slug(from)?;
        let to = slug(to)?;
        self.settings(&from)?;
        if from == DEFAULT_BOARD {
            return Err(Error::new(
                &codes::KANBAN_BOARD_EXISTS,
                "the default board cannot be renamed",
            ));
        }
        if self.dir(&to).exists() {
            return Err(Error::new(
                &codes::KANBAN_BOARD_EXISTS,
                format!("board '{to}' already exists"),
            ));
        }
        let (src, dst) = (self.dir(&from), self.dir(&to));
        fs::rename(&src, &dst).map_err(|e| Error::io(&codes::HOME_IO, &src, &e))?;
        Ok(to)
    }

    /// Archives (or restores) a board.
    ///
    /// # Errors
    ///
    /// `kanban.board_not_found`, `home.io`. The default board cannot be
    /// archived.
    pub fn set_archived(&self, slug: &str, archived: bool) -> Result<(), Error> {
        if slug == DEFAULT_BOARD && archived {
            return Err(Error::new(
                &codes::KANBAN_BOARD_EXISTS,
                "the default board cannot be archived",
            ));
        }
        let mut settings = self.settings(slug)?;
        settings.archived = archived;
        self.save_settings(slug, &settings)
    }

    /// The slugs of live boards.
    ///
    /// # Errors
    ///
    /// As [`Boards::list`].
    pub fn live(&self) -> Result<Vec<String>, Error> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|b| !b.settings.archived)
            .map(|b| b.slug)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_follow_the_pattern() {
        assert_eq!(slug("ATM10").unwrap(), "atm10");
        let err = slug("-x").unwrap_err();
        assert_eq!(
            err.message(),
            "invalid board slug '-x': 1-64 chars, lowercase alphanumerics, '-' or '_', not starting with '-' or '_'"
        );
        assert!(slug(&"a".repeat(65)).is_err());
        assert!(slug("a b").is_err());
    }
}
