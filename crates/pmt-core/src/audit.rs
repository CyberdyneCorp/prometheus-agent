//! The tamper-evident security audit log (spec: security-boundaries,
//! Tamper-evident security audit log).
//!
//! `<profile>/logs/security-audit.jsonl` holds one JSON record per line:
//! every approval decision, grant change, hardening downgrade, egress
//! denial and signature failure. Each record carries the SHA-256 of the
//! record before it (`prev`) and its own (`hash`, over the record without
//! that field), so editing, inserting or deleting a line breaks the chain
//! at that line. [`verify`] walks the chain; `prometheus security audit
//! verify` prints its answer.
//!
//! The chain detects tampering after the fact; it is not a secret (a writer
//! with access to the file can rebuild the whole chain), which is why the
//! file is owner-only like the rest of the profile.

use std::fs::File;
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::private_fs;

/// The log's path relative to the profile directory.
pub const FILE: &str = "logs/security-audit.jsonl";

/// `prev` of the first record.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// How much of the file's end is read to find the last record.
const TAIL: u64 = 256 * 1024;

/// What a record is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// An approval decision (human, transport, policy or guardian).
    Approval,
    /// A capability grant added or revoked.
    Grant,
    /// A hardening downgrade a session runs with.
    Downgrade,
    /// A connection the egress policy refused.
    EgressDenied,
    /// A signature or digest that did not verify.
    Signature,
}

impl Kind {
    /// The serialized name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Grant => "grant",
            Self::Downgrade => "downgrade",
            Self::EgressDenied => "egress_denied",
            Self::Signature => "signature",
        }
    }
}

/// One event to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// What it is about.
    pub kind: Kind,
    /// The session, when one was involved.
    pub session: Option<String>,
    /// What was decided on: a command, a capability, a host, a plugin.
    pub subject: String,
    /// The outcome: `once`, `deny`, `added`, `revoked`, `denied`...
    pub decision: String,
    /// Extra context (who decided, why).
    pub detail: Option<String>,
}

impl Event {
    /// An event without a session or detail.
    #[must_use]
    pub fn new(kind: Kind, subject: impl Into<String>, decision: impl Into<String>) -> Self {
        Self {
            kind,
            session: None,
            subject: subject.into(),
            decision: decision.into(),
            detail: None,
        }
    }

    /// With the session it happened in.
    #[must_use]
    pub fn in_session(mut self, session: impl Into<String>) -> Self {
        self.session = Some(session.into());
        self
    }

    /// With extra context.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

/// One line of the log. Field order is the serialized order, which the
/// hash covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    ts: String,
    kind: Kind,
    session: Option<String>,
    subject: String,
    decision: String,
    detail: Option<String>,
    prev: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hash: Option<String>,
}

impl Record {
    /// The SHA-256 of the record without its `hash`.
    fn digest(&self) -> String {
        let unhashed = Self {
            hash: None,
            ..self.clone()
        };
        let text = serde_json::to_string(&unhashed).unwrap_or_default();
        hex(&Sha256::digest(text.as_bytes()))
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// The audit log of one profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    /// The log of the profile at `profile_dir`.
    #[must_use]
    pub fn of(profile_dir: &Path) -> Self {
        Self::at(profile_dir.join(FILE))
    }

    /// The log at `path`.
    #[must_use]
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Its path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends `event`, chained to the last record, under the log's lock.
    ///
    /// # Errors
    ///
    /// Any I/O error; the log is a symlink.
    pub fn append(&self, event: &Event) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            private_fs::create_dir(dir)?;
        }
        let _lock = private_fs::lock_beside(&self.path)?;
        let prev = last_hash(&self.path)?;
        let mut record = Record {
            ts: jiff::Timestamp::now().to_string(),
            kind: event.kind,
            session: event.session.clone(),
            subject: event.subject.clone(),
            decision: event.decision.clone(),
            detail: event.detail.clone(),
            prev,
            hash: None,
        };
        record.hash = Some(record.digest());
        let mut line = serde_json::to_string(&record).map_err(io::Error::other)?;
        line.push('\n');
        let mut file = private_fs::append_options().open(&self.path)?;
        file.write_all(line.as_bytes())
    }

    /// Appends `event`, logging a failure at WARN: a lost audit record
    /// never stops the action it records.
    pub fn record(&self, event: &Event) {
        if let Err(err) = self.append(event) {
            tracing::warn!(
                error = %err,
                path = %self.path.display(),
                kind = event.kind.as_str(),
                "could not append to the security audit log"
            );
        }
    }

    /// Walks the chain.
    ///
    /// # Errors
    ///
    /// I/O errors other than a missing log (which verifies, empty).
    pub fn verify(&self) -> io::Result<Verdict> {
        let text = match private_fs::read_no_follow(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(err),
        };
        Ok(verify_text(&text))
    }
}

/// The answer of [`AuditLog::verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Every record chains to the one before it.
    Intact {
        /// How many records the log holds.
        records: usize,
    },
    /// The first line that does not verify.
    Broken {
        /// 1-based line number.
        line: usize,
        /// What is wrong with it.
        reason: String,
    },
}

fn verify_text(text: &str) -> Verdict {
    let mut prev = GENESIS.to_owned();
    let mut records = 0;
    for (index, line) in text.split_terminator('\n').enumerate() {
        let broken = |reason: &str| Verdict::Broken {
            line: index + 1,
            reason: reason.to_owned(),
        };
        let Ok(record) = serde_json::from_str::<Record>(line) else {
            return broken("not an audit record");
        };
        let digest = record.digest();
        if record.hash.as_deref() != Some(digest.as_str()) {
            return broken("its content does not match its hash (edited)");
        }
        if record.prev != prev {
            return broken(
                "it does not chain to the record before it (inserted, removed or reordered)",
            );
        }
        prev = digest;
        records += 1;
    }
    Verdict::Intact { records }
}

/// The hash of the log's last record, [`GENESIS`] when it has none. A last
/// line that is not a record chains by the hash of its bytes (and fails
/// verification at that line).
fn last_hash(path: &Path) -> io::Result<String> {
    let mut file = match open_no_follow(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(GENESIS.to_owned()),
        Err(err) => return Err(err),
    };
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL)))?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail)?;
    let tail = String::from_utf8_lossy(&tail);
    let Some(line) = tail.split_terminator('\n').next_back() else {
        return Ok(GENESIS.to_owned());
    };
    Ok(serde_json::from_str::<Record>(line)
        .ok()
        .and_then(|r| r.hash)
        .unwrap_or_else(|| hex(&Sha256::digest(line.as_bytes()))))
}

fn open_no_follow(path: &Path) -> io::Result<File> {
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "is a symbolic link; refusing to follow it",
        ));
    }
    File::open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log() -> (tempfile::TempDir, AuditLog) {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::of(dir.path());
        (dir, log)
    }

    fn fill(log: &AuditLog, n: usize) {
        for i in 0..n {
            log.append(
                &Event::new(Kind::Approval, format!("rm -rf build{i}"), "once").in_session("ses_1"),
            )
            .unwrap();
        }
    }

    #[test]
    fn an_untouched_log_verifies() {
        let (_dir, log) = log();
        assert_eq!(log.verify().unwrap(), Verdict::Intact { records: 0 });
        fill(&log, 3);
        assert_eq!(log.verify().unwrap(), Verdict::Intact { records: 3 });
    }

    #[test]
    fn editing_a_record_names_its_line() {
        let (_dir, log) = log();
        fill(&log, 4);
        let text = std::fs::read_to_string(log.path()).unwrap();
        let edited = text.replace("rm -rf build1", "rm -rf /tmp/elsewhere");
        std::fs::write(log.path(), edited).unwrap();
        let Verdict::Broken { line, .. } = log.verify().unwrap() else {
            panic!("edit not detected");
        };
        assert_eq!(line, 2);
    }

    #[test]
    fn removing_a_record_breaks_the_chain_at_the_next() {
        let (_dir, log) = log();
        fill(&log, 3);
        let text = std::fs::read_to_string(log.path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        std::fs::write(log.path(), format!("{}\n{}\n", lines[0], lines[2])).unwrap();
        assert!(matches!(
            log.verify().unwrap(),
            Verdict::Broken { line: 2, .. }
        ));
    }

    #[test]
    fn appending_after_a_reopen_continues_the_chain() {
        let (dir, log) = log();
        fill(&log, 2);
        let again = AuditLog::of(dir.path());
        fill(&again, 2);
        assert_eq!(again.verify().unwrap(), Verdict::Intact { records: 4 });
    }

    #[cfg(unix)]
    #[test]
    fn the_log_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_dir, log) = log();
        fill(&log, 1);
        let mode = std::fs::metadata(log.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
