//! Reading log records for `prometheus logs` (spec: cli, Log viewer;
//! observability, Log viewer): which files a component lives in, record
//! filters, the last N matching records and following a file.

use std::collections::{BTreeSet, VecDeque};
use std::fs::File;
use std::io::{self, BufRead as _, BufReader, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use pmt_core::error::{Error, codes};

use super::rotate::rotated;
use super::{FILE_NAME, HOME_COMPONENTS, KEEP, Record, home_dir, severity};

/// What `prometheus logs` selects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    /// The component (`errors` selects WARN and above of all).
    pub component: String,
    /// Least severe level shown.
    pub level: Option<String>,
    /// Records at or after this time.
    pub since: Option<Timestamp>,
    /// Records of these sessions (`--session`, or every session of the
    /// lineage for `--root`).
    pub sessions: Option<BTreeSet<String>>,
}

impl Filter {
    /// Whether `record` is selected.
    #[must_use]
    pub fn matches(&self, record: &Record) -> bool {
        self.component_matches(record)
            && self.level_matches(record)
            && self
                .since
                .is_none_or(|since| record.ts.parse::<Timestamp>().is_ok_and(|ts| ts >= since))
            && self.sessions.as_ref().is_none_or(|ids| {
                record
                    .session_id
                    .as_ref()
                    .is_some_and(|id| ids.contains(id))
            })
    }

    fn component_matches(&self, record: &Record) -> bool {
        match self.component.as_str() {
            "errors" => severity(&record.level) <= severity("warn"),
            c if HOME_COMPONENTS.contains(&c) => true,
            c => record.component == c,
        }
    }

    fn level_matches(&self, record: &Record) -> bool {
        self.level
            .as_deref()
            .is_none_or(|level| severity(&record.level) <= severity(level))
    }
}

/// The current log file of `component`: `<home>/logs/<c>.log` for the
/// home components, else the profile's `logs/prometheus.log`.
#[must_use]
pub fn current_file(home: &Path, profile_dir: &Path, component: &str) -> PathBuf {
    if HOME_COMPONENTS.contains(&component) {
        home_dir(home).join(format!("{component}.log"))
    } else {
        profile_dir.join("logs").join(FILE_NAME)
    }
}

/// Parses `--since`: `<N>[smhd]` before `now`, or an RFC 3339 time.
///
/// # Errors
///
/// `logs.invalid_since` (exit 2) with the spec message.
pub fn parse_since(value: &str, now: Timestamp) -> Result<Timestamp, Error> {
    let invalid = || {
        Error::new(
            &codes::LOGS_INVALID_SINCE,
            format!("invalid --since value '{value}': use 30m, 2h, 1d or an RFC 3339 time"),
        )
    };
    crate::time::parse_point(value, now).ok_or_else(invalid)
}

/// The last `limit` records (`0` = all) of the log at `current` and its
/// rotated files that `filter` selects, oldest first, with the files read
/// (newest first). Older rotated files are read only while more records are
/// needed. Lines that are not records are skipped.
///
/// # Errors
///
/// `logs.io` when an existing file cannot be read.
pub fn tail(
    current: &Path,
    filter: &Filter,
    limit: usize,
) -> Result<(Vec<Record>, Vec<PathBuf>), Error> {
    let mut found: VecDeque<Record> = VecDeque::new();
    let mut read = Vec::new();
    for path in generations(current) {
        if !path.is_file() {
            continue;
        }
        let records = read_records(&path)?;
        read.push(path);
        for record in records.into_iter().rev().filter(|r| filter.matches(r)) {
            found.push_front(record);
            if limit > 0 && found.len() == limit {
                return Ok((found.into(), read));
            }
        }
    }
    Ok((found.into(), read))
}

/// `current`, `current.1` ... `current.<KEEP-1>`, newest first.
fn generations(current: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    std::iter::once(current.to_owned()).chain((1..KEEP).map(|n| rotated(current, n)))
}

fn read_records(path: &Path) -> Result<Vec<Record>, Error> {
    let file = File::open(path).map_err(|err| io_error(path, &err))?;
    let mut records = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|err| io_error(path, &err))?;
        if let Ok(record) = serde_json::from_str(&line) {
            records.push(record);
        }
    }
    Ok(records)
}

/// Reads records appended to a log file after a starting point, following
/// rotations (the file is replaced) and truncations.
#[derive(Debug)]
pub struct Follower {
    path: PathBuf,
    offset: u64,
    partial: String,
    identity: Option<(u64, u64)>,
}

impl Follower {
    /// Follows `path` from its current end.
    #[must_use]
    pub fn from_end(path: &Path) -> Self {
        let offset = std::fs::metadata(path).map_or(0, |m| m.len());
        Self {
            path: path.to_owned(),
            offset,
            partial: String::new(),
            identity: File::open(path).ok().as_ref().and_then(identity),
        }
    }

    /// The complete records written since the last call.
    ///
    /// # Errors
    ///
    /// `logs.io` when the file exists but cannot be read.
    pub fn poll(&mut self) -> Result<Vec<Record>, Error> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(io_error(&self.path, &err)),
        };
        let len = file
            .metadata()
            .map_err(|err| io_error(&self.path, &err))?
            .len();
        let now = identity(&file);
        if len < self.offset || now != self.identity {
            self.identity = now;
            // Rotated or truncated: the new file is read from its start.
            self.offset = 0;
            self.partial.clear();
        }
        file.seek(SeekFrom::Start(self.offset))
            .map_err(|err| io_error(&self.path, &err))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|err| io_error(&self.path, &err))?;
        self.offset += bytes.len() as u64;
        self.partial.push_str(&String::from_utf8_lossy(&bytes));
        let Some(end) = self.partial.rfind('\n') else {
            return Ok(Vec::new());
        };
        let complete: String = self.partial.drain(..=end).collect();
        Ok(complete
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect())
    }
}

/// Device and inode, to notice a replaced file of any size.
#[cfg(unix)]
fn identity(file: &File) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    file.metadata().ok().map(|m| (m.dev(), m.ino()))
}

/// Without inode numbers a replacement is noticed when the file shrinks.
#[cfg(not(unix))]
fn identity(_file: &File) -> Option<(u64, u64)> {
    None
}

fn io_error(path: &Path, err: &io::Error) -> Error {
    Error::new(
        &codes::LOGS_IO,
        format!("cannot read {}: {err}", path.display()),
    )
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;

    use super::{Filter, Follower, parse_since, tail};
    use crate::logs::Record;
    use crate::logs::rotate::rotated;

    fn record(level: &str, component: &str, session: Option<&str>, msg: &str) -> Record {
        Record {
            ts: "2026-05-01T10:00:00.000000Z".to_owned(),
            level: level.to_owned(),
            component: component.to_owned(),
            profile: Some("default".to_owned()),
            session_id: session.map(str::to_owned),
            turn_id: None,
            trace_id: None,
            msg: msg.to_owned(),
            fields: serde_json::Map::new(),
        }
    }

    fn line(r: &Record) -> String {
        format!("{}\n", serde_json::to_string(r).unwrap())
    }

    #[test]
    fn since_accepts_durations_and_rfc3339_and_names_the_forms_otherwise() {
        let now: Timestamp = "2026-05-01T12:00:00Z".parse().unwrap();
        assert_eq!(
            parse_since("30m", now).unwrap(),
            "2026-05-01T11:30:00Z".parse::<Timestamp>().unwrap()
        );
        assert_eq!(
            parse_since("1d", now).unwrap(),
            "2026-04-30T12:00:00Z".parse::<Timestamp>().unwrap()
        );
        assert_eq!(
            parse_since("2026-05-01T08:00:00Z", now).unwrap(),
            "2026-05-01T08:00:00Z".parse::<Timestamp>().unwrap()
        );
        for bad in ["yesterday", "", "5w", "m", "-3h", "5é", "é", "3日"] {
            let err = parse_since(bad, now).unwrap_err();
            assert_eq!(
                err.message(),
                format!("invalid --since value '{bad}': use 30m, 2h, 1d or an RFC 3339 time")
            );
            assert_eq!(err.exit_code(), 2);
        }
    }

    #[test]
    fn tail_reads_rotated_files_only_while_records_are_needed() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("prometheus.log");
        std::fs::write(
            rotated(&log, 1),
            line(&record("info", "agent", None, "old")),
        )
        .unwrap();
        std::fs::write(
            &log,
            [
                line(&record("info", "agent", None, "a")),
                line(&record("warn", "cron", None, "b")),
                "not json\n".to_owned(),
                line(&record("error", "agent", Some("ses_1"), "c")),
            ]
            .concat(),
        )
        .unwrap();
        let agent = Filter {
            component: "agent".to_owned(),
            ..Filter::default()
        };
        let (records, files) = tail(&log, &agent, 2).unwrap();
        let msgs: Vec<&str> = records.iter().map(|r| r.msg.as_str()).collect();
        assert_eq!(msgs, ["a", "c"]);
        assert_eq!(files, std::slice::from_ref(&log));
        let (records, files) = tail(&log, &agent, 0).unwrap();
        assert_eq!(records.first().unwrap().msg, "old");
        assert_eq!(files, [log.clone(), rotated(&log, 1)]);
        let errors = Filter {
            component: "errors".to_owned(),
            ..Filter::default()
        };
        let msgs: Vec<String> = tail(&log, &errors, 0)
            .unwrap()
            .0
            .into_iter()
            .map(|r| r.msg)
            .collect();
        assert_eq!(msgs, ["b", "c"]);
        let session = Filter {
            component: "agent".to_owned(),
            sessions: Some(["ses_1".to_owned()].into()),
            level: Some("error".to_owned()),
            ..Filter::default()
        };
        assert_eq!(tail(&log, &session, 0).unwrap().0.len(), 1);
    }

    #[test]
    fn a_follower_sees_appended_records_and_survives_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("prometheus.log");
        std::fs::write(&log, line(&record("info", "agent", None, "before"))).unwrap();
        let mut follower = Follower::from_end(&log);
        assert!(follower.poll().unwrap().is_empty());
        let mut text = std::fs::read_to_string(&log).unwrap();
        text.push_str(&line(&record("info", "agent", None, "after")));
        text.push_str("{\"partial");
        std::fs::write(&log, &text).unwrap();
        let msgs: Vec<String> = follower
            .poll()
            .unwrap()
            .into_iter()
            .map(|r| r.msg)
            .collect();
        assert_eq!(msgs, ["after"]);
        std::fs::rename(&log, rotated(&log, 1)).unwrap();
        std::fs::write(&log, line(&record("info", "agent", None, "new"))).unwrap();
        let msgs: Vec<String> = follower
            .poll()
            .unwrap()
            .into_iter()
            .map(|r| r.msg)
            .collect();
        assert_eq!(msgs, ["new"]);
    }
}
