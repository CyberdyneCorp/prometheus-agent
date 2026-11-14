//! Attachments (spec: kanban, Attachments): bytes in the home's
//! content-addressed blob store `<home>/kanban/blobs/<aa>/<sha256>`,
//! indexed per task with a sanitized display name, and refused above
//! `kanban.attachment_max_bytes` before anything is stored.

use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use rusqlite::params;
use serde::Serialize;
use serde_json::json;
use sha2::{Digest as _, Sha256};

use crate::board::{Board, event, load};
use crate::db::sql;
use crate::model::Actor;

/// Bytes to attach.
#[derive(Debug, Clone, Copy)]
pub struct Upload<'a> {
    /// The display name (sanitized on the way in).
    pub name: &'a str,
    /// The MIME type (`None`: guessed from the name).
    pub content_type: Option<&'a str>,
    /// The bytes.
    pub bytes: &'a [u8],
}

/// One indexed attachment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Attachment {
    /// `a_<ULID>`.
    pub id: String,
    /// Its task.
    pub task_id: String,
    /// Sanitized display name.
    pub filename: String,
    /// MIME type.
    pub content_type: String,
    /// Bytes.
    pub size: u64,
    /// Content hash (the blob's name).
    pub sha256: String,
    /// Who attached it (profile or surface).
    pub uploader: String,
    /// When (Unix ms).
    pub at: i64,
}

/// The refusal above the limit.
#[must_use]
pub fn too_large(size: u64, limit: u64) -> Error {
    Error::new(
        &codes::KANBAN_ATTACHMENT_TOO_LARGE,
        format!("attachment too large: {size} > {limit}"),
    )
}

/// A display name with no directories, control characters or path
/// separators, at most 255 bytes; `attachment` when nothing is left.
#[must_use]
pub fn sanitize(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let out: String = base
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let mut out = out.trim().trim_start_matches('.').to_owned();
    while out.len() > 255 {
        out.pop();
    }
    if out.is_empty() {
        "attachment".to_owned()
    } else {
        out
    }
}

/// `<blobs>/<aa>/<sha256>`.
#[must_use]
pub fn blob_path(blobs: &Path, sha256: &str) -> PathBuf {
    blobs.join(&sha256[..2]).join(sha256)
}

/// Stores `bytes` in the blob store (temp file + rename); returns the
/// hash.
///
/// # Errors
///
/// `home.io`.
pub fn put_blob(blobs: &Path, bytes: &[u8]) -> Result<String, Error> {
    let sha = crate::model::hex(&Sha256::digest(bytes));
    let path = blob_path(blobs, &sha);
    if path.exists() {
        return Ok(sha);
    }
    let dir = path.parent().unwrap_or(blobs);
    private_fs::create_dir(dir).map_err(|e| Error::io(&codes::HOME_IO, dir, &e))?;
    private_fs::write_atomic(&path, bytes).map_err(|e| Error::io(&codes::HOME_IO, &path, &e))?;
    Ok(sha)
}

/// Reads a file of at most `limit` bytes.
///
/// # Errors
///
/// `kanban.attachment_too_large`, `home.io`.
pub fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>, Error> {
    let size = fs::metadata(path)
        .map_err(|e| Error::io(&codes::HOME_IO, path, &e))?
        .len();
    if size > limit {
        return Err(too_large(size, limit));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|f| f.take(limit + 1).read_to_end(&mut bytes))
        .map_err(|e| Error::io(&codes::HOME_IO, path, &e))?;
    let read = bytes.len() as u64;
    if read > limit {
        return Err(too_large(read, limit));
    }
    Ok(bytes)
}

/// A content type guessed from a file name.
#[must_use]
pub fn guess_type(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "json" => "application/json",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "zip" => "application/zip",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => "application/octet-stream",
    }
}

impl Board {
    /// Attaches `bytes` to `task`.
    ///
    /// # Errors
    ///
    /// `kanban.attachment_too_large` (nothing stored),
    /// `kanban.task_not_found`, `home.io`.
    pub fn attach(
        &mut self,
        actor: &Actor,
        task: &str,
        upload: &Upload<'_>,
        limit: u64,
        now: i64,
    ) -> Result<Attachment, Error> {
        let Upload {
            name,
            content_type,
            bytes,
        } = *upload;
        let size = bytes.len() as u64;
        if size > limit {
            return Err(too_large(size, limit));
        }
        load(self.conn(), task)?;
        let sha = put_blob(self.blobs(), bytes)?;
        let filename = sanitize(name);
        let attachment = Attachment {
            id: crate::model::prefixed_id("a"),
            task_id: task.to_owned(),
            content_type: content_type
                .unwrap_or_else(|| guess_type(&filename))
                .to_owned(),
            filename,
            size,
            sha256: sha,
            uploader: actor.name.clone(),
            at: now,
        };
        let label = actor.label();
        self.write(|tx| {
            insert(tx, &attachment)?;
            event(tx, Some(task), actor.run.as_deref(), &label, "attached", &json!({"attachment": attachment.id, "filename": attachment.filename, "size": size}), now)?;
            Ok(())
        })?;
        Ok(attachment)
    }

    /// Attaches the file at `path`, refusing it above `limit` before
    /// reading it.
    ///
    /// # Errors
    ///
    /// As [`Board::attach`].
    pub fn attach_file(
        &mut self,
        actor: &Actor,
        task: &str,
        path: &Path,
        limit: u64,
        now: i64,
    ) -> Result<Attachment, Error> {
        let bytes = read_limited(path, limit)?;
        let name = path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let upload = Upload {
            name: &name,
            content_type: None,
            bytes: &bytes,
        };
        self.attach(actor, task, &upload, limit, now)
    }

    /// A task's attachments, oldest first.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn attachments(&self, task: &str) -> Result<Vec<Attachment>, Error> {
        let mut stmt = self
            .conn()
            .prepare(
                "SELECT id, task_id, filename, content_type, size, sha256, uploader, at \
                 FROM attachments WHERE task_id = ?1 ORDER BY at, id",
            )
            .map_err(sql)?;
        let rows = stmt
            .query_map([task], |r| {
                Ok(Attachment {
                    id: r.get(0)?,
                    task_id: r.get(1)?,
                    filename: r.get(2)?,
                    content_type: r.get(3)?,
                    size: r.get::<_, i64>(4).map(|s| u64::try_from(s).unwrap_or(0))?,
                    sha256: r.get(5)?,
                    uploader: r.get(6)?,
                    at: r.get(7)?,
                })
            })
            .map_err(sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql)?;
        Ok(rows)
    }

    /// The blob file of an attachment.
    #[must_use]
    pub fn attachment_path(&self, attachment: &Attachment) -> PathBuf {
        blob_path(self.blobs(), &attachment.sha256)
    }
}

pub(crate) fn insert(tx: &rusqlite::Transaction<'_>, a: &Attachment) -> Result<(), Error> {
    tx.execute(
        "INSERT INTO attachments(id, task_id, filename, content_type, size, sha256, uploader, at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            a.id,
            a.task_id,
            a.filename,
            a.content_type,
            i64::try_from(a.size).unwrap_or(i64::MAX),
            a.sha256,
            a.uploader,
            a.at
        ],
    )
    .map_err(sql)?;
    Ok(())
}
