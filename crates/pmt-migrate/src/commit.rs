//! Committing planned steps into a Prometheus profile, shared by the
//! Hermes migration and the other-agent importers: the destination, its
//! staging directory, per-component commits recorded in the ledger
//! (spec: hermes-migration, Per-component atomicity and resumability) and
//! the saved report.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::error::{Error, ErrorKind, codes};
use pmt_store::Store;

use crate::component::Component;
use crate::host::Host;
use crate::ledger::{self, Ledger};
use crate::plan::{Dest, Existing, Step};
use crate::report::{self, ComponentReport, ProfileReport, Report, Status};
use crate::undo::Undo;

/// Each component's plan, in commit order.
pub type Planned = Vec<(Component, Result<Box<dyn Step>, Error>)>;

/// The opened destination of a real run.
pub struct Target {
    /// The profile directory.
    pub profile: pmt_config::ProfileDir,
    /// Its store.
    pub store: Store,
}

/// Opens (creating it when missing) the destination profile.
///
/// # Errors
///
/// `migrate.destination` (exit 73) for an I/O or permission failure,
/// other home and store errors as they are.
pub fn open_target(prometheus_home: &Path, name: &ProfileName) -> Result<Target, Error> {
    let open = || -> Result<Target, Error> {
        let home = Home::open(prometheus_home)?;
        let profile = home.profile(name)?;
        let scope = ProfileScope::open(profile.clone(), &Overrides::default(), Launch::Other)?;
        let store = Store::open(&scope)?;
        Ok(Target { profile, store })
    };
    open().map_err(|e| {
        if matches!(e.kind(), ErrorKind::Io | ErrorKind::PermissionDenied) {
            Error::new(
                &codes::MIGRATE_DESTINATION,
                format!(
                    "cannot write the Prometheus profile {name}: {}",
                    e.message()
                ),
            )
        } else {
            e
        }
    })
}

/// The staging directory: `<profile>/cache/migrate-<run-id>/` on a real
/// run, a temporary directory outside the Prometheus home on a dry run;
/// removed when dropped.
///
/// # Errors
///
/// `home.io` when it cannot be created.
pub fn staging_dir(dest: Option<&Target>, run_id: &str) -> Result<Staging, Error> {
    match dest {
        Some(t) => {
            let dir = t
                .profile
                .path()
                .join("cache")
                .join(format!("migrate-{run_id}"));
            std::fs::create_dir_all(&dir)
                .map_err(|e| Error::io_doing(&codes::HOME_IO, "cannot create", &dir, &e))?;
            Ok(Staging::Owned(dir))
        }
        None => tempfile::Builder::new()
            .prefix("prometheus-migrate-")
            .tempdir()
            .map(Staging::Temp)
            .map_err(|e| {
                Error::new(
                    &codes::HOME_IO,
                    format!("cannot create a staging directory: {e}"),
                )
            }),
    }
}

/// A staging directory, removed when dropped.
pub enum Staging {
    /// Inside the destination profile.
    Owned(PathBuf),
    /// Outside the Prometheus home (dry run).
    Temp(tempfile::TempDir),
}

impl Staging {
    /// The directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Owned(p) => p,
            Self::Temp(t) => t.path(),
        }
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if let Self::Owned(dir) = self {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Records `component`'s outcome; several steps of one component add up
/// in one component report.
pub fn record(
    out: &mut ProfileReport,
    component: Component,
    status: Status,
    error: Option<String>,
    step: Option<&mut Box<dyn Step>>,
) {
    if let Some(step) = step {
        out.warnings.extend(step.warnings());
        out.entries.append(step.entries());
    }
    let counts = report::counts(&out.entries, component);
    match out.components.iter_mut().find(|c| c.component == component) {
        Some(c) => {
            c.counts = counts;
            if rank(status) > rank(c.status) {
                c.status = status;
                c.error = error;
            }
        }
        None => out.components.push(ComponentReport {
            component,
            status,
            error,
            counts,
        }),
    }
}

/// Which status wins when several steps of one component end
/// differently.
fn rank(status: Status) -> u8 {
    match status {
        Status::Planned => 0,
        Status::Committed => 1,
        Status::Interrupted => 2,
        Status::Failed => 3,
    }
}

/// A dry run's steps, with the conflicts an existing destination profile
/// would cause.
pub fn finish_planned(steps: Planned, existing: Option<&Existing<'_>>, out: &mut ProfileReport) {
    for (component, step) in steps {
        match step {
            Ok(mut step) => {
                if let Some(existing) = existing {
                    step.predict(existing);
                }
                record(out, component, Status::Planned, None, Some(&mut step));
            }
            Err(e) => record(
                out,
                component,
                Status::Failed,
                Some(e.message().to_owned()),
                None,
            ),
        }
    }
}

/// Committing a real run's components into one profile.
pub struct Commit<'a> {
    /// The destination.
    pub target: &'a Target,
    /// `--overwrite`.
    pub overwrite: bool,
    /// The binary's side.
    pub host: &'a dyn Host,
    /// The staging directory.
    pub staging: &'a Path,
    /// This run's id.
    pub run_id: &'a str,
}

impl Commit<'_> {
    /// The profile's ledger of `source` for the source directory `dir`
    /// (and a warning when it was written for another source).
    ///
    /// # Errors
    ///
    /// As [`Ledger::load`].
    pub fn ledger(
        &self,
        source: &str,
        dir: &Path,
    ) -> Result<(RefCell<Ledger>, Option<String>), Error> {
        let (ledger, warning) = Ledger::load(
            self.target.profile.path(),
            source,
            dir,
            &ledger::identity(dir),
        )?;
        Ok((RefCell::new(ledger), warning))
    }

    /// Commits the components in order; each committed one is recorded in
    /// the ledger at once, so an interruption or a later failure leaves it
    /// complete and a re-run resumes after it. A component that does not
    /// commit is undone: neither its writes nor its ledger records stay.
    pub fn all(&self, ledger: &RefCell<Ledger>, steps: Planned, out: &mut ProfileReport) {
        let mut stopped = false;
        for (component, step) in steps {
            stopped |= self.host.interrupted();
            if stopped {
                record(out, component, Status::Interrupted, None, None);
                continue;
            }
            let mut step = match step {
                Ok(step) => step,
                Err(e) => {
                    record(
                        out,
                        component,
                        Status::Failed,
                        Some(e.message().to_owned()),
                        None,
                    );
                    continue;
                }
            };
            let Err(e) = self.one(ledger, component, &mut step) else {
                record(out, component, Status::Committed, None, Some(&mut step));
                continue;
            };
            let interrupted = e.code().code == codes::MIGRATE_INTERRUPTED.code;
            stopped |= interrupted;
            if !step.commits_in_parts() {
                mark_undone(step.entries(), interrupted);
            }
            let (status, error) = if interrupted {
                (Status::Interrupted, None)
            } else {
                (Status::Failed, Some(e.message().to_owned()))
            };
            record(out, component, status, error, Some(&mut step));
        }
    }

    /// Applies and commits one component; when that fails, a component
    /// committing in parts keeps the parts it committed (recorded for the
    /// re-run), any other is undone.
    fn one(
        &self,
        ledger: &RefCell<Ledger>,
        component: Component,
        step: &mut Box<dyn Step>,
    ) -> Result<(), Error> {
        let before = ledger.borrow().clone();
        let undo = RefCell::new(Undo::new(
            self.staging.join("undo").join(component.as_str()),
        ));
        let Err(e) = self
            .apply(ledger, &undo, step)
            .and_then(|()| ledger.borrow_mut().commit(component.as_str(), self.run_id))
        else {
            return Ok(());
        };
        if step.commits_in_parts() {
            let _ = ledger.borrow().save();
            return Err(e);
        }
        *ledger.borrow_mut() = before;
        match undo.into_inner().rollback(&self.target.store) {
            Ok(()) => Err(e),
            Err(u) => Err(Error::new(
                e.code(),
                format!(
                    "{}; undoing its partial writes failed: {}",
                    e.message(),
                    u.message()
                ),
            )),
        }
    }

    fn apply(
        &self,
        ledger: &RefCell<Ledger>,
        undo: &RefCell<Undo>,
        step: &mut Box<dyn Step>,
    ) -> Result<(), Error> {
        // Each component sees the config the previous ones wrote.
        let scope = ProfileScope::open(
            self.target.profile.clone(),
            &Overrides::default(),
            Launch::Other,
        )?;
        let dest = Dest {
            scope: &scope,
            store: &self.target.store,
            overwrite: self.overwrite,
            host: self.host,
            staging: self.staging.to_owned(),
            ledger,
            undo,
        };
        step.apply(&dest)
    }
}

/// Nothing of an undone component is reported as imported.
fn mark_undone(entries: &mut [report::Entry], interrupted: bool) {
    let why = if interrupted {
        "interrupted before the component committed; nothing of it was written"
    } else {
        "the component failed; nothing of it was written"
    };
    for e in entries {
        if !matches!(
            e.disposition,
            report::Disposition::Failed
                | report::Disposition::Unsupported
                | report::Disposition::Skipped
        ) {
            e.disposition = report::Disposition::Failed;
            why.clone_into(&mut e.reason);
        }
    }
}

/// Saves `report` (one profile) to `<profile>/migrations/<prefix>-<run-id>.json`.
pub fn save(target: &Target, prefix: &str, report: &Report) {
    let path = target
        .profile
        .path()
        .join("migrations")
        .join(format!("{prefix}-{}.json", report.run_id));
    let text = serde_json::to_string_pretty(report).unwrap_or_default();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = pmt_core::private_fs::write_atomic(&path, text.as_bytes()) {
        tracing::warn!("cannot save the migration report {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use pmt_core::error::{Error, codes};

    use super::*;
    use crate::report::{Disposition, Entry};
    use crate::testkit::{FakeHost, target};
    use crate::writes::Writes;

    /// Writes config, a secret, a file and a store row, then fails.
    struct HalfWay {
        entries: Vec<Entry>,
    }

    impl Step for HalfWay {
        fn component(&self) -> Component {
            Component::Config
        }

        fn entries(&mut self) -> &mut Vec<Entry> {
            &mut self.entries
        }

        fn apply(&mut self, dest: &Dest<'_>) -> Result<(), Error> {
            let mut w = Writes::default();
            w.key("agent.max_iterations", 90, 0);
            w.secret("HALF_WAY_KEY", "sk-half", 0);
            w.apply(dest, &mut self.entries)?;
            let file = dest.scope.dir().join("notes/kept.md");
            dest.keep(&file)?;
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, "written\n").unwrap();
            let before = dest.store.pairing_row("telegram", "42")?;
            dest.store
                .adopt_pairing("telegram", "42", None, jiff::Timestamp::now())?;
            dest.on_undo(move |s| s.restore_pairing("telegram", "42", before.as_ref()));
            Err(Error::new(&codes::HOME_IO, "disk full"))
        }
    }

    #[test]
    fn a_component_failing_after_partial_writes_leaves_nothing_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let t = target(tmp.path());
        let dir = t.profile.path().to_owned();
        let staging = tmp.path().join("staging");
        let commit = Commit {
            target: &t,
            overwrite: false,
            host: &FakeHost,
            staging: &staging,
            run_id: "run1",
        };
        let (ledger, _) = commit.ledger("hermes", tmp.path()).unwrap();
        let entry = Entry::new(Component::Config, "max_turns", Disposition::Mapped);
        let step: Box<dyn Step> = Box::new(HalfWay {
            entries: vec![entry],
        });
        let mut out = ProfileReport {
            hermes_profile: "default".to_owned(),
            target: None,
            source: String::new(),
            error: None,
            code: None,
            components: Vec::new(),
            entries: Vec::new(),
            warnings: Vec::new(),
        };
        let config_before = std::fs::read_to_string(dir.join("config.toml")).ok();
        commit.all(&ledger, vec![(Component::Config, Ok(step))], &mut out);

        assert_eq!(out.components[0].status, Status::Failed);
        assert_eq!(out.entries[0].disposition, Disposition::Failed);
        assert!(out.entries[0].reason.contains("nothing of it was written"));
        assert_eq!(
            std::fs::read_to_string(dir.join("config.toml")).ok(),
            config_before
        );
        let scope =
            ProfileScope::open(t.profile.clone(), &Overrides::default(), Launch::Other).unwrap();
        assert!(
            scope
                .secrets()
                .store()
                .get("HALF_WAY_KEY")
                .unwrap()
                .is_none()
        );
        assert!(!dir.join("notes").exists());
        assert!(!t.store.is_paired("telegram", "42").unwrap());
        let ledger = ledger.into_inner();
        assert!(ledger.get("config:agent.max_iterations").is_none());
        assert!(ledger.committed("config").is_none());
        let (saved, _) = commit.ledger("hermes", tmp.path()).unwrap();
        assert!(saved.into_inner().get("secret:HALF_WAY_KEY").is_none());
    }
}
