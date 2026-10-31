//! Update receipts (spec: distribution-and-update, Update receipts at the
//! root home): one JSON file per update, check, refusal or rollback under
//! `<home>/updates/receipts/` (never a profile directory), named
//! `<timestamp>-<id>.json`, plus `latest.json`; the newest 50 are kept.

use std::path::{Path, PathBuf};

use pmt_core::error::Error;
use pmt_core::private_fs;
use pmt_core::runtime::process_start_time;
use serde::{Deserialize, Serialize};

use crate::install::{create_dirs, io_error};

/// How many receipts are kept.
pub const KEEP: usize = 50;
/// The copy of the newest receipt.
pub const LATEST_FILE: &str = "latest.json";

/// What a receipt records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// `prometheus update`.
    Update,
    /// `prometheus update --check` or a background check.
    Check,
    /// `prometheus update --rollback`.
    Rollback,
    /// `prometheus update --set-channel`.
    SetChannel,
}

/// The receipt's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// In progress (or its process died: see [`Outcome::Interrupted`]).
    Running,
    /// Done; for a check, the check completed.
    Success,
    /// Failed (after any rollback).
    Failed,
    /// Refused before any change (owner elsewhere, lock held).
    Refused,
    /// `--rollback` restored the previous binary.
    RolledBack,
    /// Its process went away while it was running.
    Interrupted,
}

impl Outcome {
    /// The receipt spelling (`success`, `rolled_back`, ...).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Refused => "refused",
            Self::RolledBack => "rolled_back",
            Self::Interrupted => "interrupted",
        }
    }
}

/// A pre-update snapshot taken for one profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRef {
    /// The profile.
    pub profile: String,
    /// `snap_<ULID>`.
    pub id: String,
    /// RFC 3339 creation time.
    pub created_at: String,
}

/// One receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateReceipt {
    /// Unique id (ULID).
    pub id: String,
    /// What ran.
    pub kind: Kind,
    /// How it ended.
    pub outcome: Outcome,
    /// RFC 3339 start time.
    pub started_at: String,
    /// RFC 3339 end time.
    pub finished_at: Option<String>,
    /// Process that wrote it.
    pub pid: u32,
    /// Start time of that process (Unix s), to tell a dead writer.
    pub pid_started: u64,
    /// The binary it concerns.
    pub binary: PathBuf,
    /// The installed version before.
    pub from: String,
    /// The version installed, found or restored.
    pub to: Option<String>,
    /// The channel used.
    pub channel: String,
    /// Stages completed, in order (`check`, `download`, `verify`,
    /// `self-test`, `snapshot`, `swap`, ...).
    pub stages: Vec<String>,
    /// Follow-ups left for the user.
    pub follow_ups: Vec<String>,
    /// Error messages.
    pub errors: Vec<String>,
    /// Pre-update snapshots taken (one per profile).
    #[serde(default)]
    pub snapshots: Vec<SnapshotRef>,
}

/// A receipt being written: every change is persisted at once, so a crash
/// leaves a `running` receipt the next update marks `interrupted`.
#[derive(Debug)]
pub struct ReceiptWriter {
    dir: PathBuf,
    path: PathBuf,
    receipt: UpdateReceipt,
}

impl ReceiptWriter {
    /// Starts a `running` receipt under `home`.
    ///
    /// # Errors
    ///
    /// `update.failed` when the receipt cannot be written.
    pub fn begin(
        home: &Path,
        kind: Kind,
        binary: &Path,
        from: &str,
        channel: &str,
    ) -> Result<Self, Error> {
        let dir = dir(home);
        create_dirs(&dir)?;
        let now = jiff::Timestamp::now();
        let id = ulid::Ulid::generate().to_string();
        let pid = std::process::id();
        let path = dir.join(format!("{}-{id}.json", now.strftime("%Y%m%dT%H%M%S%.3fZ")));
        let receipt = UpdateReceipt {
            id,
            kind,
            outcome: Outcome::Running,
            started_at: now.to_string(),
            finished_at: None,
            pid,
            pid_started: process_start_time(pid).unwrap_or(0),
            binary: binary.to_owned(),
            from: from.to_owned(),
            to: None,
            channel: channel.to_owned(),
            stages: Vec::new(),
            follow_ups: Vec::new(),
            errors: Vec::new(),
            snapshots: Vec::new(),
        };
        let writer = Self { dir, path, receipt };
        writer.save()?;
        Ok(writer)
    }

    /// The receipt so far.
    #[must_use]
    pub fn receipt(&self) -> &UpdateReceipt {
        &self.receipt
    }

    /// The receipt file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Changes the receipt and persists it.
    ///
    /// # Errors
    ///
    /// `update.failed` when the receipt cannot be written.
    pub fn update(&mut self, change: impl FnOnce(&mut UpdateReceipt)) -> Result<(), Error> {
        change(&mut self.receipt);
        self.save()
    }

    /// Records a completed stage.
    ///
    /// # Errors
    ///
    /// `update.failed` when the receipt cannot be written.
    pub fn stage(&mut self, stage: &str) -> Result<(), Error> {
        self.update(|r| r.stages.push(stage.to_owned()))
    }

    /// Ends the receipt with `outcome` (plus the error for a failure) and
    /// prunes old receipts.
    ///
    /// # Errors
    ///
    /// `update.failed` when the receipt cannot be written.
    pub fn finish(
        mut self,
        outcome: Outcome,
        error: Option<&Error>,
    ) -> Result<UpdateReceipt, Error> {
        self.receipt.outcome = outcome;
        self.receipt.finished_at = Some(jiff::Timestamp::now().to_string());
        if let Some(error) = error {
            self.receipt.errors.push(error.message().to_owned());
        }
        self.save()?;
        prune(&self.dir)?;
        Ok(self.receipt)
    }

    fn save(&self) -> Result<(), Error> {
        let json = serde_json::to_vec_pretty(&self.receipt).expect("receipt serializes");
        private_fs::write_atomic(&self.path, &json).map_err(|e| io_error(&self.path, &e))?;
        let latest = self.dir.join(LATEST_FILE);
        private_fs::write_atomic(&latest, &json).map_err(|e| io_error(&latest, &e))
    }
}

/// `<home>/updates/receipts`.
#[must_use]
pub fn dir(home: &Path) -> PathBuf {
    home.join("updates").join("receipts")
}

/// Every receipt, oldest first.
///
/// # Errors
///
/// `update.failed` when the directory cannot be read.
pub fn list(home: &Path) -> Result<Vec<(PathBuf, UpdateReceipt)>, Error> {
    let dir = dir(home);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_error(&dir, &e)),
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| is_receipt(p))
        .collect();
    paths.sort();
    Ok(paths
        .into_iter()
        .filter_map(|p| read(&p).map(|r| (p, r)))
        .collect())
}

/// `latest.json`, if any.
#[must_use]
pub fn latest(home: &Path) -> Option<UpdateReceipt> {
    read(&dir(home).join(LATEST_FILE))
}

/// The newest successful update, whose snapshots a rollback restores.
///
/// # Errors
///
/// `update.failed` when the receipts cannot be read.
pub fn last_successful_update(home: &Path) -> Result<Option<UpdateReceipt>, Error> {
    Ok(list(home)?
        .into_iter()
        .rev()
        .map(|(_, r)| r)
        .find(|r| r.kind == Kind::Update && r.outcome == Outcome::Success))
}

/// Marks every `running` receipt whose process is gone `interrupted`;
/// returns how many.
///
/// # Errors
///
/// `update.failed` when a receipt cannot be rewritten.
///
/// # Panics
///
/// Never: receipts always serialize.
pub fn mark_interrupted(home: &Path) -> Result<usize, Error> {
    let mut marked = 0;
    let receipts = list(home)?;
    let newest = receipts.last().map(|(p, _)| p.clone());
    for (path, mut receipt) in receipts {
        let alive = process_start_time(receipt.pid) == Some(receipt.pid_started);
        if receipt.outcome != Outcome::Running || alive {
            continue;
        }
        receipt.outcome = Outcome::Interrupted;
        let json = serde_json::to_vec_pretty(&receipt).expect("receipt serializes");
        private_fs::write_atomic(&path, &json).map_err(|e| io_error(&path, &e))?;
        if newest.as_ref() == Some(&path) {
            let latest = dir(home).join(LATEST_FILE);
            private_fs::write_atomic(&latest, &json).map_err(|e| io_error(&latest, &e))?;
        }
        marked += 1;
    }
    Ok(marked)
}

fn prune(dir: &Path) -> Result<(), Error> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| io_error(dir, &e))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| is_receipt(p))
        .collect();
    paths.sort();
    let excess = paths.len().saturating_sub(KEEP);
    for path in &paths[..excess] {
        std::fs::remove_file(path).map_err(|e| io_error(path, &e))?;
    }
    Ok(())
}

fn is_receipt(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    name != LATEST_FILE
        && Path::new(name).extension().is_some_and(|e| e == "json")
        && name.starts_with(|c: char| c.is_ascii_digit())
}

fn read(path: &Path) -> Option<UpdateReceipt> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::{KEEP, Kind, Outcome, ReceiptWriter, dir, latest, list, mark_interrupted};

    #[test]
    fn running_receipt_of_a_dead_process_becomes_interrupted() {
        let home = tempfile::tempdir().unwrap();
        let writer =
            ReceiptWriter::begin(home.path(), Kind::Update, "/bin/p".as_ref(), "v1", "stable")
                .unwrap();
        let mut receipt = writer.receipt().clone();
        // A pid that is not running (pid_started never matches).
        receipt.pid_started = 1;
        receipt.pid = u32::MAX - 1;
        std::fs::write(writer.path(), serde_json::to_vec(&receipt).unwrap()).unwrap();
        assert_eq!(mark_interrupted(home.path()).unwrap(), 1);
        assert_eq!(latest(home.path()).unwrap().outcome, Outcome::Interrupted);
    }

    #[test]
    fn live_running_receipt_is_left_alone() {
        let home = tempfile::tempdir().unwrap();
        let _writer =
            ReceiptWriter::begin(home.path(), Kind::Update, "/bin/p".as_ref(), "v1", "stable")
                .unwrap();
        assert_eq!(mark_interrupted(home.path()).unwrap(), 0);
    }

    #[test]
    fn only_the_newest_fifty_are_kept() {
        let home = tempfile::tempdir().unwrap();
        for _ in 0..KEEP + 3 {
            let writer =
                ReceiptWriter::begin(home.path(), Kind::Check, "/bin/p".as_ref(), "v1", "stable")
                    .unwrap();
            writer.finish(Outcome::Success, None).unwrap();
        }
        assert_eq!(list(home.path()).unwrap().len(), KEEP);
        assert!(dir(home.path()).join("latest.json").is_file());
    }
}
