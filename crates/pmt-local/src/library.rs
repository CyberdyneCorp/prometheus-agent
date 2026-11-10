//! The machine-wide model library (spec: local-models, Model library):
//!
//! ```text
//! <home>/models/<id>/model.json   the record: files, source, window
//! <home>/models/<id>/<files>      staged weights, parts, projector, draft
//! ```
//!
//! A model is usable once its record exists, which happens only after
//! every file of its download job was verified and published; until then
//! the library lists it as `partial` from its job. A linked model's record
//! points at the user's own file, which `rm` never touches.

use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

use crate::download::{Jobs, io_error};

/// The record file in each model directory.
pub const RECORD_FILE: &str = "model.json";

/// What a file is to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Weights (the first part of a split model).
    Weights,
    /// A further part of a split model.
    Part,
    /// The vision projector.
    Projector,
    /// A draft model for speculative decoding.
    Draft,
}

/// One file of a model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFile {
    /// Absolute path.
    pub path: PathBuf,
    /// What it is.
    pub role: Role,
    /// Size in bytes.
    pub size: u64,
}

/// Where a model came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The curated catalog.
    Catalog,
    /// A Hugging Face search result.
    Hub,
    /// `prometheus local add`: the user's own file, not copied.
    Linked,
}

/// A library record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    /// Library id.
    pub id: String,
    /// Where it came from.
    pub source: Source,
    /// Its files.
    pub files: Vec<ModelFile>,
    /// The quantisation, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quant: Option<String>,
    /// The window the managed server starts it at.
    pub context: u64,
    /// The native window from the file's own metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_max: Option<u64>,
    /// RFC 3339 time it entered the library.
    pub added_at: String,
}

impl Model {
    /// The weights file (the first part of a split model).
    #[must_use]
    pub fn weights(&self) -> Option<&Path> {
        self.file(Role::Weights)
    }

    /// The vision projector.
    #[must_use]
    pub fn projector(&self) -> Option<&Path> {
        self.file(Role::Projector)
    }

    fn file(&self, role: Role) -> Option<&Path> {
        self.files
            .iter()
            .find(|f| f.role == role)
            .map(|f| f.path.as_path())
    }
}

/// One row of the library listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Listed {
    /// Library id.
    pub id: String,
    /// `ready`, `partial` (a download not finished and verified) or
    /// `missing` (a linked file that is gone).
    pub state: &'static str,
    /// Where it came from (`null` while partial).
    pub source: Option<Source>,
    /// Bytes of its files.
    pub size: u64,
    /// The download job of a partial model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
}

/// The library of one home.
#[derive(Debug, Clone)]
pub struct Library {
    root: PathBuf,
}

impl Library {
    /// `<home>/models`.
    #[must_use]
    pub fn open(home: &Path) -> Self {
        Self {
            root: home.join("models"),
        }
    }

    /// The library directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory of model `id` (staged files go here).
    #[must_use]
    pub fn dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    /// Every usable model, by id.
    #[must_use]
    pub fn models(&self) -> Vec<Model> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut models: Vec<Model> = entries
            .flatten()
            .filter_map(|e| read(&e.path().join(RECORD_FILE)))
            .collect();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        models
    }

    /// The usable model `id`.
    ///
    /// # Errors
    ///
    /// `local.not_found` (66), naming the download job when it is partial.
    pub fn model(&self, id: &str, jobs: &Jobs) -> Result<Model, Error> {
        if let Some(model) = read(&self.dir(id).join(RECORD_FILE)) {
            return Ok(model);
        }
        let partial = jobs.unfinished(id);
        let message = match &partial {
            Some(job) => format!(
                "local model '{id}' is partial: its download (job {}) has not finished",
                job.id
            ),
            None => format!("no local model '{id}' in the library"),
        };
        Err(
            Error::new(&codes::LOCAL_NOT_FOUND, message).with_hint(match partial {
                Some(job) => format!("prometheus local resume {}", job.id),
                None => format!("prometheus local pull {id}"),
            }),
        )
    }

    /// The listing: usable models, then partial downloads.
    #[must_use]
    pub fn list(&self, jobs: &Jobs) -> Vec<Listed> {
        let mut rows: Vec<Listed> = self
            .models()
            .into_iter()
            .map(|m| Listed {
                state: if m.files.iter().all(|f| f.path.is_file()) {
                    "ready"
                } else {
                    "missing"
                },
                source: Some(m.source),
                size: m.files.iter().map(|f| f.size).sum(),
                id: m.id,
                job: None,
            })
            .collect();
        for job in jobs.all() {
            let model = job.subject != "engine" && !job.subject.starts_with("engine ");
            if model && job.state != crate::download::JobState::Done {
                if rows.iter().any(|r| r.id == job.subject) {
                    continue;
                }
                rows.push(Listed {
                    id: job.subject.clone(),
                    state: "partial",
                    source: None,
                    size: job.total_bytes(),
                    job: Some(job.id.clone()),
                });
            }
        }
        rows
    }

    /// Writes the record of `model`, making it usable.
    ///
    /// # Errors
    ///
    /// `runtime.io`.
    ///
    /// # Panics
    ///
    /// Never: the record serializes.
    pub fn publish(&self, model: &Model) -> Result<(), Error> {
        let dir = self.dir(&model.id);
        std::fs::create_dir_all(&dir).map_err(|e| io_error(&dir, &e))?;
        let path = dir.join(RECORD_FILE);
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_vec_pretty(model).expect("a record serializes");
        std::fs::write(&tmp, text).map_err(|e| io_error(&tmp, &e))?;
        std::fs::rename(&tmp, &path).map_err(|e| io_error(&path, &e))
    }

    /// Links the user's GGUF file at `path` into the library without
    /// copying it; the id is the file stem.
    ///
    /// # Errors
    ///
    /// `config.invalid_argument` (2) for a file that is not GGUF;
    /// `runtime.io`.
    pub fn link(
        &self,
        path: &Path,
        context: impl Fn(&crate::gguf::Info) -> u64,
    ) -> Result<Model, Error> {
        let path = std::path::absolute(path).map_err(|e| io_error(path, &e))?;
        let info = crate::gguf::read(&path).map_err(|reason| {
            Error::new(
                &codes::CONFIG_INVALID_ARGUMENT,
                format!("{} is not a usable GGUF file: {reason}", path.display()),
            )
        })?;
        let id = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let size = std::fs::metadata(&path)
            .map_err(|e| io_error(&path, &e))?
            .len();
        let model = Model {
            id,
            source: Source::Linked,
            files: vec![ModelFile {
                path: path.clone(),
                role: Role::Weights,
                size,
            }],
            quant: None,
            context: context(&info),
            context_max: info.context_length,
            added_at: jiff::Timestamp::now().to_string(),
        };
        self.publish(&model)?;
        Ok(model)
    }

    /// Removes model `id`: its staged files (weights, parts, projector,
    /// draft), its record and any unfinished download of it. A linked
    /// original is left where it is.
    ///
    /// # Errors
    ///
    /// `local.not_found` (66) when there is nothing of that name;
    /// `runtime.io`.
    pub fn remove(&self, id: &str, jobs: &Jobs) -> Result<(), Error> {
        let dir = self.dir(id);
        let job = jobs.unfinished(id);
        if !dir.exists() && job.is_none() {
            return Err(Error::new(
                &codes::LOCAL_NOT_FOUND,
                format!("no local model '{id}' in the library"),
            ));
        }
        if let Some(job) = job {
            jobs.remove(&job)?;
        }
        if dir.exists() {
            // Only files inside the model's own directory are deleted; a
            // linked file lives elsewhere and is never followed.
            std::fs::remove_dir_all(&dir).map_err(|e| io_error(&dir, &e))?;
        }
        Ok(())
    }
}

fn read(path: &Path) -> Option<Model> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}
