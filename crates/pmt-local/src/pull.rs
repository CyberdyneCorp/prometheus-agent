//! Pulling a model into the library: a download job whose `meta` is the
//! library record to publish once every file is verified and in place, so
//! `resume` needs nothing but the job.

use pmt_core::error::{Error, codes};

use crate::catalog::{Catalog, Entry, Priced};
use crate::download::{Control, Downloader, FileSpec, Finished, Job, Jobs, Progress};
use crate::library::{Library, Model, ModelFile, Role, Source};

/// How a pull run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pulled {
    /// The model is in the library.
    Ready(Model),
    /// Paused; the job id resumes it.
    Paused(String),
}

/// The job of catalog entry `entry` priced as `priced` (its quantisation
/// and starting window): the existing unfinished one, else a new one.
///
/// # Errors
///
/// `runtime.io`.
///
/// # Panics
///
/// Never: a priced quant is one of the entry's.
pub fn catalog_job(
    catalog: &Catalog,
    entry: &Entry,
    priced: &Priced,
    library: &Library,
    jobs: &Jobs,
) -> Result<Job, Error> {
    if let Some(job) = jobs.unfinished(&entry.id) {
        return Ok(job);
    }
    let quant = entry
        .quants
        .iter()
        .find(|q| q.quant == priced.quant)
        .expect("a priced quant is one of the entry's");
    let dir = library.dir(&entry.id);
    let mut files: Vec<(FileSpec, Role)> = quant
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let role = if i == 0 { Role::Weights } else { Role::Part };
            (
                spec(catalog.url(entry, f), &f.path, f.size, &f.sha256, &dir),
                role,
            )
        })
        .collect();
    if let Some(p) = &entry.projector {
        files.push((
            spec(catalog.url(entry, p), &p.path, p.size, &p.sha256, &dir),
            Role::Projector,
        ));
    }
    let model = Model {
        id: entry.id.clone(),
        source: Source::Catalog,
        files: files
            .iter()
            .map(|(s, role)| ModelFile {
                path: s.target.clone(),
                role: *role,
                size: s.size,
            })
            .collect(),
        quant: Some(quant.quant.clone()),
        context: priced.context_start,
        context_max: Some(entry.context_max),
        added_at: String::new(),
    };
    create(jobs, &model, files.into_iter().map(|(s, _)| s).collect())
}

/// A new job publishing `model` once `files` are in place.
///
/// # Errors
///
/// `runtime.io`.
///
/// # Panics
///
/// Never: the record serializes.
pub fn create(jobs: &Jobs, model: &Model, files: Vec<FileSpec>) -> Result<Job, Error> {
    let meta = serde_json::to_value(model).expect("a record serializes");
    jobs.create(&model.id, files, meta)
}

/// A file of a model, staged into the model's directory.
#[must_use]
pub fn spec(url: String, path: &str, size: u64, sha256: &str, dir: &std::path::Path) -> FileSpec {
    let name = path.rsplit('/').next().unwrap_or(path).to_owned();
    FileSpec {
        target: dir.join(&name),
        name,
        url,
        size,
        sha256: sha256.to_owned(),
    }
}

/// Runs a model job; once done, publishes its record and deletes the job.
///
/// # Errors
///
/// The downloader's errors; `local.not_found` for a job that is not a
/// model's.
pub async fn run(
    library: &Library,
    jobs: &Jobs,
    downloader: &Downloader,
    job: &mut Job,
    control: &Control,
    progress: &mut (dyn FnMut(Progress) + Send),
) -> Result<Pulled, Error> {
    let mut model: Model = serde_json::from_value(job.meta.clone()).map_err(|_| {
        Error::new(
            &codes::LOCAL_NOT_FOUND,
            format!("download job '{}' is not a model download", job.id),
        )
    })?;
    match downloader.run(jobs, job, control, progress).await? {
        Finished::Paused => Ok(Pulled::Paused(job.id.clone())),
        Finished::Done => {
            model.added_at = jiff::Timestamp::now().to_string();
            if model.context == 0 {
                // A hub model's window comes from its own metadata.
                let info = model.weights().and_then(|w| crate::gguf::read(w).ok());
                model.context_max = info.as_ref().and_then(|i| i.context_length);
                model.context = start_window(model.context_max);
            }
            library.publish(&model)?;
            jobs.remove(job)?;
            Ok(Pulled::Ready(model))
        }
    }
}

/// The window a model without catalog pricing starts at: the guaranteed
/// minimum, or its native window when that is smaller.
#[must_use]
pub fn start_window(native: Option<u64>) -> u64 {
    native.map_or(crate::catalog::MIN_WINDOW, |n| {
        n.min(crate::catalog::MIN_WINDOW)
    })
}
