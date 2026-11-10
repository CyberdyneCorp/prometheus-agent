//! The curator operations of one profile, shared by `prometheus curator`
//! and `/curator` (spec: skills, Curator CLI; /curator slash command;
//! Curator audit ledger, single-edit rollback, purge and reports).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use jiff::Timestamp;
use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_skills::Catalog;
use pmt_skills::bundled::Bundled;
use pmt_skills::catalog::Provenance;
use pmt_skills::curator::moves::{self, Archived};
use pmt_skills::curator::report::RunReport;
use pmt_skills::curator::run::{self, Options};
use pmt_skills::curator::{self, Scope, backup};
use pmt_skills::ledger::{Actor, Evidence, Ledger};
use pmt_skills::roots::{Tier, profile_skills};
use pmt_skills::scan::PatternScanner;
use pmt_store::Store;
use pmt_store::skill_ledger::Entry;
use pmt_store::skill_usage::CreatedBy;
use serde::Serialize;

use super::review::ModelReview;

/// The curator of one profile.
pub(crate) struct Curator {
    pub(crate) scope: ProfileScope,
    pub(crate) store: Arc<Store>,
    pub(crate) launch: LaunchEnv,
}

/// `curator status`.
#[derive(Debug, Serialize)]
pub(crate) struct Status {
    pub(crate) enabled: bool,
    pub(crate) paused: bool,
    pub(crate) last_run_at: Option<String>,
    pub(crate) interval_secs: u64,
    pub(crate) managed: Vec<String>,
    pub(crate) pinned: Vec<String>,
    pub(crate) archived: usize,
    pub(crate) planned: Vec<curator::Transition>,
}

fn refused(message: String) -> Error {
    Error::new(&codes::CURATOR_REFUSED, message)
}

impl Curator {
    /// The curator of `scope`.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub(crate) fn open(scope: ProfileScope, launch: LaunchEnv) -> Result<Self, Error> {
        let store = Arc::new(Store::open(&scope)?);
        Ok(Self {
            scope,
            store,
            launch,
        })
    }

    fn catalog(&self) -> Catalog {
        pmt_skills::eject::profile_catalog(
            &self.scope,
            None,
            self.launch.user_home().map(PathBuf::as_path),
            Bundled::builtin(),
        )
    }

    /// Skills any scheduled job names (paused ones included). Fails
    /// closed: without the jobs the curator cannot tell which skills they
    /// use, so it refuses rather than archive one of them.
    fn cron_skills(&self) -> Result<BTreeSet<String>, Error> {
        let rows = self.store.cron_jobs().map_err(|e| {
            refused(format!(
                "cannot read the scheduled jobs, so the skills they use are unknown: {}",
                e.message()
            ))
        })?;
        Ok(rows
            .into_iter()
            .filter_map(|row| pmt_cron::job::Job::from_row(row).ok())
            .flat_map(|job| job.spec.skills)
            .collect())
    }

    fn skills_dir(&self) -> PathBuf {
        profile_skills(self.scope.dir())
    }

    fn ledger(&self) -> Ledger<'_> {
        Ledger::new(
            &self.store,
            &self.skills_dir(),
            self.scope.config().skills.ledger_max_bytes,
        )
    }

    fn with_scope<T>(&self, now: Timestamp, f: impl FnOnce(&Scope<'_>) -> T) -> Result<T, Error> {
        let catalog = self.catalog();
        let cron = self.cron_skills()?;
        let scope = Scope {
            profile_dir: self.scope.dir(),
            store: &self.store,
            config: self.scope.config(),
            catalog: &catalog,
            cron_skills: &cron,
            now,
        };
        Ok(f(&scope))
    }

    /// `curator status`.
    ///
    /// # Errors
    ///
    /// `curator.refused` (scheduled jobs unreadable), store errors.
    pub(crate) fn status(&self) -> Result<Status, Error> {
        let state = self.store.curator_state()?;
        let config = &self.scope.config().curator;
        let (managed, plan) = self.with_scope(Timestamp::now(), |scope| {
            let (managed, _) = curator::managed(scope);
            (managed, curator::plan(scope))
        })?;
        let pinned = self
            .store
            .skill_usages()?
            .into_iter()
            .filter(|u| u.pinned)
            .map(|u| u.name)
            .collect();
        Ok(Status {
            enabled: config.enabled,
            paused: state.paused,
            last_run_at: state.last_run_at.map(|t| t.to_string()),
            interval_secs: config.interval_secs,
            managed: managed.into_iter().map(|m| m.skill.name.clone()).collect(),
            pinned,
            archived: moves::archived(&self.skills_dir()).len(),
            planned: plan.transitions,
        })
    }

    /// One pass (consolidating with the `auxiliary.curator` model when
    /// asked or configured).
    ///
    /// # Errors
    ///
    /// `curator.busy`, `curator.refused` (scheduled jobs unreadable),
    /// `skill.io`, store errors.
    pub(crate) fn run(
        &self,
        dry_run: bool,
        consolidate: bool,
        automatic: bool,
    ) -> Result<RunReport, Error> {
        let consolidate = consolidate || self.scope.config().curator.consolidate;
        let review = ModelReview::new(&self.scope, &self.store, &self.launch);
        let options = Options {
            dry_run,
            consolidate,
            automatic,
        };
        self.with_scope(Timestamp::now(), |scope| {
            run::run(scope, options, Some(&review), &PatternScanner)
        })?
    }

    /// `curator pause` / `resume`.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub(crate) fn pause(&self, paused: bool) -> Result<String, Error> {
        self.store.set_curator_paused(paused)?;
        Ok(if paused {
            "Curator paused: no automatic pass runs until `curator resume`.".to_owned()
        } else {
            "Curator resumed.".to_owned()
        })
    }

    /// The local profile skill `name` (what pin, adopt and archive act on).
    fn local(&self, name: &str) -> Result<Arc<pmt_skills::Skill>, Error> {
        let skill = self.catalog().lookup(name).map_err(|_| {
            Error::new(
                &codes::SKILL_NOT_FOUND,
                format!("no skill '{name}' in this profile"),
            )
        })?;
        if skill.tier != Tier::Profile || skill.provenance != Provenance::Local {
            return Err(refused(format!(
                "'{name}' is a {} skill; the curator manages only the profile's own skills",
                match (skill.tier, skill.provenance) {
                    (_, Provenance::Hub(_)) => "hub",
                    (Tier::Bundled, _) | (_, Provenance::Builtin) => "bundled",
                    (Tier::Plugin, _) => "plugin",
                    _ => skill.tier.as_str(),
                }
            )));
        }
        Ok(skill)
    }

    /// `curator pin|unpin <name>`.
    ///
    /// # Errors
    ///
    /// `skill.not_found`, store errors.
    pub(crate) fn pin(&self, name: &str, pinned: bool) -> Result<String, Error> {
        let skill = self.catalog().lookup(name).map_err(|_| {
            Error::new(
                &codes::SKILL_NOT_FOUND,
                format!("no skill '{name}' in this profile"),
            )
        })?;
        self.store.set_skill_pinned(&skill.name, pinned)?;
        Ok(if pinned {
            format!("Pinned '{}': the curator leaves it alone.", skill.name)
        } else {
            format!("Unpinned '{}'.", skill.name)
        })
    }

    /// `curator adopt <name>`.
    ///
    /// # Errors
    ///
    /// `skill.not_found`, `curator.refused`, store errors.
    pub(crate) fn adopt(&self, name: &str) -> Result<String, Error> {
        let skill = self.local(name)?;
        self.store.set_skill_adopted(&skill.name, true)?;
        Ok(format!(
            "Adopted '{}': the curator manages it from now on.",
            skill.name
        ))
    }

    /// `curator list-unmanaged`: the profile's own skills the curator
    /// leaves alone because nobody gave them to it.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub(crate) fn unmanaged(&self) -> Result<Vec<String>, Error> {
        let catalog = self.catalog();
        let mut out = Vec::new();
        for skill in &catalog.skills {
            if skill.tier != Tier::Profile || skill.provenance != Provenance::Local {
                continue;
            }
            let managed = self
                .store
                .skill_usage(&skill.name)?
                .is_some_and(|u| u.created_by == CreatedBy::Agent || u.adopted);
            if !managed {
                out.push(skill.name.clone());
            }
        }
        Ok(out)
    }

    /// `curator list-archived`.
    #[must_use]
    pub(crate) fn archived(&self) -> Vec<Archived> {
        moves::archived(&self.skills_dir())
    }

    /// `curator archive <name>`.
    ///
    /// # Errors
    ///
    /// `curator.refused` (pinned, not the profile's own), `skill.io`.
    pub(crate) fn archive(&self, name: &str) -> Result<String, Error> {
        let skill = self.local(name)?;
        if self
            .store
            .skill_usage(&skill.name)?
            .is_some_and(|u| u.pinned)
        {
            return Err(refused(format!(
                "'{}' is pinned; unpin it first",
                skill.name
            )));
        }
        let dir = skill
            .dir
            .path()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default();
        let rel = dir
            .strip_prefix(self.skills_dir())
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .map_err(|_| refused(format!("'{name}' is outside the skills directory")))?;
        let to = moves::archive(
            &self.ledger(),
            &self.store,
            &skill.name,
            &rel,
            Actor::User,
            &Evidence::default(),
        )?;
        Ok(format!("Archived '{}' to skills/{to}.", skill.name))
    }

    /// `curator restore <name>`.
    ///
    /// # Errors
    ///
    /// `curator.refused`, `skill.not_found`, `skill.io`.
    pub(crate) fn restore(&self, name: &str) -> Result<String, Error> {
        moves::restore(
            &self.ledger(),
            &self.store,
            &self.catalog(),
            name,
            Actor::User,
        )?;
        Ok(format!(
            "Restored '{name}'; it loads from the next session."
        ))
    }

    /// `curator prune --days <n>`: archives managed skills idle that long
    /// (pinned and job-referenced ones are not managed); the archived
    /// ones (or, dry, the candidates).
    ///
    /// # Errors
    ///
    /// `curator.refused` (scheduled jobs unreadable), `skill.io`.
    pub(crate) fn prune(&self, days: u64, dry_run: bool) -> Result<Vec<(String, i64)>, Error> {
        let now = Timestamp::now();
        let candidates = self.with_scope(now, |scope| {
            let (managed, _) = curator::managed(scope);
            managed
                .into_iter()
                .filter_map(|m| {
                    let idle = now.duration_since(m.last_activity).as_secs() / 86_400;
                    (u64::try_from(idle).unwrap_or(0) >= days).then_some((m, idle))
                })
                .collect::<Vec<_>>()
        })?;
        let ledger = self.ledger();
        let mut out = Vec::new();
        for (m, idle) in candidates {
            if !dry_run {
                moves::archive(
                    &ledger,
                    &self.store,
                    &m.skill.name,
                    &m.rel,
                    Actor::User,
                    &Evidence::default(),
                )?;
            }
            out.push((m.skill.name.clone(), idle));
        }
        Ok(out)
    }

    /// `curator purge [--days N]`: deletes archived skills older than
    /// `--days`, else `curator.archive_ttl_secs` (0: purging is off).
    ///
    /// # Errors
    ///
    /// `cli.usage` when no age is set, `skill.io`.
    pub(crate) fn purge(&self, days: Option<u64>, dry_run: bool) -> Result<Vec<Archived>, Error> {
        let age = days.map_or(self.scope.config().curator.archive_ttl_secs, |d| {
            d.saturating_mul(86_400)
        });
        if age == 0 {
            return Err(Error::new(
                &codes::CLI_USAGE,
                "purging is off: set curator.archive_ttl_secs or pass --days <n>",
            ));
        }
        let now = Timestamp::now();
        if dry_run {
            return Ok(moves::expired(&self.skills_dir(), age, now));
        }
        moves::purge(&self.ledger(), age, now)
    }

    /// `curator backup`.
    ///
    /// # Errors
    ///
    /// `skill.io`.
    pub(crate) fn backup(&self) -> Result<PathBuf, Error> {
        let keep = usize::try_from(self.scope.config().curator.backup_keep).unwrap_or(2);
        backup::write(&self.skills_dir(), keep, Timestamp::now())
    }

    /// `curator rollback <entry-id>` (one ledger entry) or, without one,
    /// the newest backup (after a fresh safety backup).
    ///
    /// # Errors
    ///
    /// `curator.refused` (unknown entry, failed safety record, no
    /// backup), `curator.busy`, `skill.io`.
    pub(crate) fn rollback(&self, entry: Option<&str>) -> Result<String, Error> {
        let _lock = self.with_scope(Timestamp::now(), run::lock)??;
        if let Some(label) = entry {
            let id = pmt_store::skill_ledger::parse_label(label).ok_or_else(|| {
                Error::new(
                    &codes::CLI_USAGE,
                    format!("'{label}' is not a ledger entry id (L<n>)"),
                )
            })?;
            let undone = self.ledger().rollback(id)?;
            return Ok(format!(
                "Rolled back L{id} ({} {} of '{}'): {} file(s) restored.",
                undone.actor,
                undone.action,
                undone.skill,
                undone.files.len()
            ));
        }
        let newest = backup::list(&self.skills_dir())
            .pop()
            .ok_or_else(|| refused("no curator backup to roll back to".to_owned()))?;
        let safety = self.backup()?;
        backup::restore(&self.skills_dir(), &newest)?;
        Ok(format!(
            "Restored the skills tree from {} (the state before it is in {}).",
            newest.display(),
            safety.display()
        ))
    }

    /// `curator ledger [--skill S] [--limit N]`.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub(crate) fn entries(&self, skill: Option<&str>, limit: usize) -> Result<Vec<Entry>, Error> {
        self.store.skill_ledger(skill, limit)
    }
}

/// The ledger rows as text.
#[must_use]
pub(crate) fn ledger_text(entries: &[Entry]) -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    for e in entries {
        let extra = e
            .absorbed_into
            .as_deref()
            .map(|into| format!("  -> absorbed into '{into}'"))
            .unwrap_or_default();
        let _ = writeln!(
            text,
            "{:<6} {} {:<8} {:<11} {} ({} file(s)){extra}",
            e.label(),
            e.at.strftime("%Y-%m-%d %H:%M"),
            e.actor,
            e.action,
            e.skill,
            e.files.len()
        );
    }
    if text.is_empty() {
        text.push_str("The skill ledger is empty.\n");
    }
    text
}
