//! What a job does before its first rollout (spec: experience-library,
//! Domain resolution for a job; Exposure to proposers; Stream order
//! within a domain; Restore on resume and repair): classify a job that
//! names no domain, wait for its domain's turn, repair the library and
//! take the job's snapshot of it.

use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};

use super::ports::SessionKind;
use super::prompts::{self, CLASSIFIER_SYSTEM};
use super::run::{Run, SessionCall};
use crate::experience::domain::{self, Source};
use crate::experience::{Library, Settings, ops};
use crate::job::Jobs;
use crate::job::task::TaskMeta;

/// `<job>/library.taken`: the snapshot was taken (with or without
/// entries), so a resumed job never takes a newer one.
const TAKEN: &str = "library.taken";

/// Takes `<profile>/evolve/domains/<domain>.lock`, waiting for a job of
/// the same domain in another process to finish.
///
/// # Errors
///
/// `harness.io`.
pub(super) async fn domain_lock(profile_dir: &Path, domain: &str) -> Result<std::fs::File, Error> {
    let dir = profile_dir.join("evolve").join("domains");
    let path = dir.join(format!("{domain}.lock"));
    let io = |e: &std::io::Error| Error::io(&codes::HARNESS_IO, &path, e);
    pmt_core::private_fs::create_dir(&dir).map_err(|e| io(&e))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| io(&e))?;
    let locked = pmt_core::spawn::blocking(move || pmt_core::file_lock::lock(&file).map(|()| file))
        .await
        .map_err(|e| Error::new(&codes::HARNESS_IO, e.to_string()))?;
    locked.map_err(|e| io(&e))
}

impl Run<'_> {
    /// The library settings of the job's profile.
    pub(crate) fn settings(&self) -> Settings {
        Settings::of(
            self.engine.config,
            self.engine.experience,
            self.engine.rules.redactor.clone(),
        )
    }

    /// Resolves the domain of a job that names none with one auxiliary
    /// call choosing among the profile's domains plus `general` (no call
    /// when `general` is the only choice), and records it.
    pub(super) async fn classify(&mut self) -> Result<(), Error> {
        let mut known: Vec<String> = Library::of(self.engine.profile_dir)
            .domains()
            .into_iter()
            .filter(|d| d != domain::GENERAL)
            .collect();
        let chosen = if known.is_empty() {
            domain::GENERAL.to_owned()
        } else {
            known.push(domain::GENERAL.to_owned());
            self.ask_classifier(&known).await
        };
        Jobs::of(self.engine.profile_dir).set_domain(&self.job.id, &chosen)?;
        let path = self.dir.join("task").join("task.toml");
        let mut meta: TaskMeta = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default();
        meta.domain.clone_from(&chosen);
        meta.domain_source = Source::Classifier;
        let text =
            toml::to_string(&meta).map_err(|e| Error::new(&codes::HARNESS_IO, e.to_string()))?;
        pmt_core::private_fs::write_atomic(&path, text.as_bytes())
            .map_err(|e| Error::io(&codes::HARNESS_IO, &path, &e))?;
        tracing::info!(job = %self.job.id, domain = %chosen, "evolution job domain classified");
        self.job.domain = chosen;
        Ok(())
    }

    async fn ask_classifier(&self, known: &[String]) -> String {
        let Ok(workspace) = self.scratch("classify") else {
            return domain::GENERAL.to_owned();
        };
        let context = super::evidence::context(&workspace);
        let built = pmt_core::private_fs::create_dir(&context).and_then(|()| {
            std::fs::copy(
                self.dir.join("task").join("task.md"),
                context.join("task.md"),
            )
        });
        let mission = prompts::classification(known);
        let reply = match built {
            Ok(_) => {
                self.session(SessionCall {
                    label: "classify",
                    kind: SessionKind::Classify,
                    system: CLASSIFIER_SYSTEM,
                    mission: &mission,
                    workspace: &workspace,
                })
                .await
            }
            Err(err) => Err(super::run::SessionError::Failed {
                message: err.to_string(),
                usd: 0.0,
            }),
        };
        let _ = std::fs::remove_dir_all(&workspace);
        match reply {
            Ok(done) => domain::classified(&done.text, known),
            Err(err) => {
                tracing::warn!(job = %self.job.id, "domain classification failed ({err:?}); using general");
                domain::GENERAL.to_owned()
            }
        }
    }

    /// Repairs the domain's library from adopted proposals, makes sure it
    /// has its `INDEX.md`, and returns the job's snapshot: taken now on a
    /// first start, the one already taken on a resume.
    pub(super) fn prepare_library(&self) -> Result<Option<PathBuf>, Error> {
        let profile = self.engine.profile_dir;
        let domain = &self.job.domain;
        let restored = ops::rebuild(profile, &self.settings(), Some(domain))?;
        if !restored.is_empty() {
            tracing::info!(job = %self.job.id, ?restored, "experience entries restored");
        }
        let snapshot = self.dir.join("library");
        if !self.dir.join(TAKEN).exists() {
            let _ = std::fs::remove_dir_all(&snapshot);
            let max = usize::try_from(self.engine.experience.max_entries_in_evidence)
                .unwrap_or(usize::MAX);
            Library::of(profile).snapshot(domain, &snapshot, max)?;
            std::fs::write(self.dir.join(TAKEN), jiff::Timestamp::now().to_string())
                .map_err(|e| Error::io(&codes::HARNESS_IO, &self.dir, &e))?;
        }
        Ok(snapshot.is_dir().then_some(snapshot))
    }
}
