//! The `.pmtrec` session recording (spec: replay-and-evaluation, Session
//! recordings): a zstd-compressed tar whose `manifest.json` lists the
//! sha256 of every other member, so a recording that was changed after
//! export is refused before anything is replayed.
//!
//! Members (see [`member`]): `state.json` (the initial state), per session
//! `sessions/<id>/trajectory.jsonl`, `prompts.json`, `calls.json` and
//! `tools.jsonl`, the responses to each request hash in call order under
//! `responses/<hex>.json` and its canonical request under
//! `requests/<hex>.json`, the `workspace/manifest.json` and, only with
//! `--include-workspace`, `workspace/files/<path>`.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::path::Path;

use pmt_core::error::{Error, codes};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The recording format this build writes and reads.
pub const FORMAT_VERSION: u32 = 1;

/// The file extension.
pub const EXTENSION: &str = "pmtrec";

const MANIFEST: &str = "manifest.json";

/// Member names.
pub mod member {
    /// The initial state.
    pub const STATE: &str = "state.json";
    /// The workspace manifest (path, size, sha256).
    pub const WORKSPACE: &str = "workspace/manifest.json";

    /// A session's trajectory.
    #[must_use]
    pub fn trajectory(session: &str) -> String {
        format!("sessions/{session}/trajectory.jsonl")
    }

    /// A session's prompt builds.
    #[must_use]
    pub fn prompts(session: &str) -> String {
        format!("sessions/{session}/prompts.json")
    }

    /// A session's model calls in order (request hashes).
    #[must_use]
    pub fn calls(session: &str) -> String {
        format!("sessions/{session}/calls.json")
    }

    /// A session's tool results.
    #[must_use]
    pub fn tools(session: &str) -> String {
        format!("sessions/{session}/tools.jsonl")
    }

    /// The responses of `request_hash`, in call order (a retried request
    /// failed before it answered).
    #[must_use]
    pub fn response(request_hash: &str) -> String {
        format!("responses/{}.json", super::hex_of(request_hash))
    }

    /// The canonical request of `request_hash`.
    #[must_use]
    pub fn request(request_hash: &str) -> String {
        format!("requests/{}.json", super::hex_of(request_hash))
    }

    /// A workspace file's contents.
    #[must_use]
    pub fn workspace_file(path: &str) -> String {
        format!("workspace/files/{path}")
    }
}

fn hex_of(request_hash: &str) -> &str {
    request_hash.strip_prefix("sha256:").unwrap_or(request_hash)
}

/// `manifest.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// [`FORMAT_VERSION`].
    pub format_version: u32,
    /// The build that recorded it.
    pub prometheus_version: String,
    /// The trajectory grammar (`<major>.<minor>`).
    pub trajectory_grammar: String,
    /// The recorded sessions, the replayed root first.
    pub session_ids: Vec<String>,
    /// RFC 3339 export time.
    pub created_at: String,
    /// `member -> sha256:<hex>` of every other member.
    pub sha256: BTreeMap<String, String>,
}

/// A recording in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recording {
    /// The manifest (its hashes are filled in by [`Recording::write`]).
    pub manifest: Manifest,
    members: BTreeMap<String, Vec<u8>>,
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let hex = digest.iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    });
    format!("sha256:{hex}")
}

fn malformed(why: impl std::fmt::Display) -> Error {
    Error::new(
        &codes::REPLAY_MALFORMED,
        format!("not a readable recording: {why}"),
    )
}

fn io(path: &Path, err: &std::io::Error) -> Error {
    Error::io(&codes::REPLAY_IO, path, err)
}

impl Recording {
    /// An empty recording of `session_ids` (the root first).
    #[must_use]
    pub fn new(
        prometheus_version: &str,
        trajectory_grammar: &str,
        session_ids: Vec<String>,
    ) -> Self {
        Self {
            manifest: Manifest {
                format_version: FORMAT_VERSION,
                prometheus_version: prometheus_version.to_owned(),
                trajectory_grammar: trajectory_grammar.to_owned(),
                session_ids,
                created_at: jiff::Timestamp::now().to_string(),
                sha256: BTreeMap::new(),
            },
            members: BTreeMap::new(),
        }
    }

    /// The replayed root session.
    #[must_use]
    pub fn root(&self) -> Option<&str> {
        self.manifest.session_ids.first().map(String::as_str)
    }

    /// Adds (or replaces) `name`.
    pub fn add(&mut self, name: &str, bytes: Vec<u8>) {
        self.members.insert(name.to_owned(), bytes);
    }

    /// Adds `name` as pretty JSON.
    ///
    /// # Panics
    ///
    /// Never: the values recorded are plain data.
    pub fn add_json(&mut self, name: &str, value: &impl Serialize) {
        let bytes = serde_json::to_vec_pretty(value).expect("plain data serializes");
        self.add(name, bytes);
    }

    /// The bytes of `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.members.get(name).map(Vec::as_slice)
    }

    /// `name` parsed as JSON; `None` when it is absent.
    ///
    /// # Errors
    ///
    /// `replay.malformed` when it does not parse.
    pub fn json<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>, Error> {
        self.get(name)
            .map(|bytes| {
                serde_json::from_slice(bytes).map_err(|e| malformed(format!("{name}: {e}")))
            })
            .transpose()
    }

    /// Every member name and its bytes.
    pub fn members(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.members.iter().map(|(k, v)| (k.as_str(), v.as_slice()))
    }

    /// Rewrites every member with `f` (export-time redaction); the
    /// manifest hashes are computed at write.
    pub fn map_members(&mut self, mut f: impl FnMut(&str, Vec<u8>) -> Vec<u8>) {
        let members = std::mem::take(&mut self.members);
        self.members = members
            .into_iter()
            .map(|(name, bytes)| {
                let mapped = f(&name, bytes);
                (name, mapped)
            })
            .collect();
    }

    /// Writes the recording to `path` (through a temp file), with the
    /// manifest listing every member's hash.
    ///
    /// # Errors
    ///
    /// `replay.io`.
    pub fn write(&mut self, path: &Path) -> Result<(), Error> {
        self.manifest.sha256 = self
            .members
            .iter()
            .map(|(name, bytes)| (name.clone(), sha256(bytes)))
            .collect();
        let temp = pmt_core::private_fs::temp_beside(path).map_err(|e| io(path, &e))?;
        let written = self
            .write_tar(&temp)
            .and_then(|()| std::fs::rename(&temp, path).map_err(|e| io(path, &e)));
        if written.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        written
    }

    fn write_tar(&self, path: &Path) -> Result<(), Error> {
        let file = pmt_core::private_fs::new_file_options()
            .open(path)
            .map_err(|e| io(path, &e))?;
        let encoder =
            zstd::Encoder::new(std::io::BufWriter::new(file), 3).map_err(|e| io(path, &e))?;
        let mut tar = tar::Builder::new(encoder);
        let manifest = serde_json::to_vec_pretty(&self.manifest).expect("a manifest serializes");
        let members = std::iter::once((MANIFEST, manifest.as_slice()))
            .chain(self.members.iter().map(|(k, v)| (k.as_str(), v.as_slice())));
        for (name, bytes) in members {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o600);
            header.set_entry_type(tar::EntryType::Regular);
            tar.append_data(&mut header, name, bytes)
                .map_err(|e| io(path, &e))?;
        }
        let mut out = tar
            .into_inner()
            .and_then(zstd::Encoder::finish)
            .map_err(|e| io(path, &e))?;
        out.flush().map_err(|e| io(path, &e))
    }

    /// Reads and verifies the recording at `path`.
    ///
    /// # Errors
    ///
    /// `replay.io` when it cannot be read; `replay.malformed` for anything
    /// but a recording of a known format; `replay.integrity` naming the
    /// first member that is missing, unlisted or changed.
    pub fn open(path: &Path) -> Result<Self, Error> {
        let file = std::fs::File::open(path).map_err(|e| io(path, &e))?;
        let decoder = zstd::Decoder::new(file).map_err(malformed)?;
        let mut tar = tar::Archive::new(decoder);
        let mut manifest: Option<Manifest> = None;
        let mut members = BTreeMap::new();
        for entry in tar.entries().map_err(malformed)? {
            let mut entry = entry.map_err(malformed)?;
            let name = entry
                .path()
                .map_err(malformed)?
                .to_string_lossy()
                .into_owned();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).map_err(malformed)?;
            if name == MANIFEST {
                manifest = Some(serde_json::from_slice(&bytes).map_err(malformed)?);
            } else {
                members.insert(name, bytes);
            }
        }
        let manifest = manifest.ok_or_else(|| malformed("no manifest.json"))?;
        if manifest.format_version > FORMAT_VERSION {
            return Err(malformed(format!(
                "format_version {} is newer than this build reads ({FORMAT_VERSION})",
                manifest.format_version
            )));
        }
        verify(&manifest, &members)?;
        Ok(Self { manifest, members })
    }
}

/// Every listed member present with its hash, and nothing unlisted.
fn verify(manifest: &Manifest, members: &BTreeMap<String, Vec<u8>>) -> Result<(), Error> {
    let failed = |member: &str| {
        Error::new(
            &codes::REPLAY_INTEGRITY,
            format!("recording integrity check failed: {member}"),
        )
    };
    for (name, expected) in &manifest.sha256 {
        match members.get(name) {
            Some(bytes) if sha256(bytes) == *expected => {}
            _ => return Err(failed(name)),
        }
    }
    match members
        .keys()
        .find(|name| !manifest.sha256.contains_key(*name))
    {
        Some(extra) => Err(failed(extra)),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_recording_reads_back_and_a_changed_hash_is_named() {
        let dir =
            tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")).unwrap();
        let path = dir.path().join("s.pmtrec");
        let mut recording = Recording::new("0.1.0", "1.0", vec!["ses_a".into()]);
        recording.add(&member::trajectory("ses_a"), b"{}\n".to_vec());
        recording.add_json(member::STATE, &serde_json::json!({"source": "cli"}));
        recording.write(&path).unwrap();
        let read = Recording::open(&path).unwrap();
        assert_eq!(read.root(), Some("ses_a"));
        assert_eq!(
            read.get("sessions/ses_a/trajectory.jsonl"),
            Some(&b"{}\n"[..])
        );
        assert_eq!(read.manifest.sha256.len(), 2);

        let mut manifest = read.manifest.clone();
        manifest
            .sha256
            .insert(member::STATE.to_owned(), sha256(b"other"));
        let err = verify(&manifest, &read.members).unwrap_err();
        assert_eq!(err.code().code, "replay.integrity");
        assert_eq!(
            err.message(),
            "recording integrity check failed: state.json"
        );
        manifest.sha256.remove(member::STATE);
        let err = verify(&manifest, &read.members).unwrap_err();
        assert_eq!(
            err.message(),
            "recording integrity check failed: state.json"
        );
    }

    #[test]
    fn a_file_that_is_not_a_recording_is_malformed() {
        let dir =
            tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")).unwrap();
        let path = dir.path().join("x.pmtrec");
        std::fs::write(&path, b"plain text").unwrap();
        let err = Recording::open(&path).unwrap_err();
        assert_eq!(err.code().code, "replay.malformed");
    }
}
