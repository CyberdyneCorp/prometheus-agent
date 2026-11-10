//! Building a `.pmtrec` recording from a profile (spec:
//! replay-and-evaluation, Session recordings): the session and its
//! compression continuations (`--lineage`: every descendant too), each
//! with its trajectory, prompt builds, model calls and tool results, the
//! initial state and the workspace manifest (contents only with
//! `--include-workspace`), every member redacted like a trajectory.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::Path;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_eval::recording::{Recording, member};
use pmt_obs::recording::{self, Journal};
use pmt_obs::trajectory::files;
use pmt_store::Store;
use pmt_tools::snapshots::{SnapshotStore, journal};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::State;

/// At most this many workspace files are listed (the manifest then says
/// `truncated: true`), so a session started in a huge tree (a home
/// directory) still exports in bounded time and size. Documented in
/// docs/guide/replay.md.
const MAX_WORKSPACE_FILES: usize = 10_000;

/// `replay export` flags.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// `--include-workspace`.
    pub include_workspace: bool,
    /// `--lineage`.
    pub lineage: bool,
}

fn io_error(path: &Path, err: &std::io::Error) -> Error {
    Error::io(&codes::REPLAY_IO, path, err)
}

fn not_recorded(session: SessionId) -> Error {
    Error::new(
        &codes::REPLAY_NOT_RECORDED,
        format!("session {session} has no recording"),
    )
    .with_hint("sessions are recorded while `observability.trajectories.enabled` is on")
}

/// The recording of `session` in the profile.
///
/// # Errors
///
/// `replay.not_recorded`, store and file errors.
pub fn recording(
    scope: &ProfileScope,
    store: &Store,
    session: SessionId,
    options: Options,
) -> Result<Recording, Error> {
    let ids = sessions(store, session, options.lineage)?;
    let names: Vec<String> = ids.iter().map(ToString::to_string).collect();
    let mut out = Recording::new(
        crate::build_info::VERSION,
        &format!(
            "{}.{}",
            pmt_obs::GRAMMAR_VERSION,
            pmt_obs::trajectory::GRAMMAR_MINOR
        ),
        names,
    );
    let mut root_journal = None;
    let mut responses: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for id in &ids {
        let journal = add_session(&mut out, scope, &id.to_string())?;
        for call in journal.iter().flat_map(|j| &j.calls) {
            let outcomes = responses.entry(call.request_hash.clone()).or_default();
            outcomes.push(call.response.clone());
        }
        if *id == session {
            root_journal = journal;
        }
    }
    for (hash, outcomes) in &responses {
        out.add_json(&member::response(hash), outcomes);
    }
    let journal = root_journal.ok_or_else(|| not_recorded(session))?;
    let row = store.session(session)?;
    let mut state = state(scope, store, &row, &journal)?;
    state.workspace_included = options.include_workspace;
    let workspace = row.cwd.as_deref().map(Path::new);
    let before = workspace.map(|root| before_session(scope, &ids, root));
    add_workspace(
        &mut out,
        workspace,
        &before.unwrap_or_default(),
        options.include_workspace,
    )?;
    out.add_json(member::STATE, &state);
    redact(scope, &mut out)?;
    Ok(out)
}

/// `session` and its compression continuations, then (with `lineage`)
/// every other descendant.
fn sessions(store: &Store, session: SessionId, lineage: bool) -> Result<Vec<SessionId>, Error> {
    let chain = store.lineage(session)?;
    let from = chain.iter().position(|id| *id == session).unwrap_or(0);
    let mut ids: Vec<SessionId> = chain[from..].to_vec();
    if lineage {
        for node in store.tree(session)? {
            if !ids.contains(&node.id) {
                ids.push(node.id);
            }
        }
    }
    Ok(ids)
}

/// Adds one session's members; returns its journal (`None`: it has
/// none, as a session without a recording).
fn add_session(
    out: &mut Recording,
    scope: &ProfileScope,
    id: &str,
) -> Result<Option<Journal>, Error> {
    let dir = scope.path(files::DIR);
    if let Some(path) = files::find(&dir, id) {
        let mut text = Vec::new();
        files::open(&path)
            .and_then(|mut reader| reader.read_to_end(&mut text))
            .map_err(|e| io_error(&path, &e))?;
        out.add(&member::trajectory(id), text);
    }
    let journal = recording::read(scope.dir(), id)
        .map_err(|e| io_error(&recording::path(scope.dir(), id), &e))?;
    let Some(journal) = journal else {
        return Ok(None);
    };
    out.add_json(&member::prompts(id), &journal.prompts);
    let calls: Vec<Value> = journal
        .calls
        .iter()
        .map(|c| json!({"request_hash": c.request_hash, "ts": c.ts, "served": c.served}))
        .collect();
    out.add_json(&member::calls(id), &calls);
    for call in &journal.calls {
        out.add_json(&member::request(&call.request_hash), &call.request);
    }
    let mut tools = String::new();
    for tool in &journal.tools {
        tools.push_str(&serde_json::to_string(tool).unwrap_or_default());
        tools.push('\n');
    }
    out.add(&member::tools(id), tools.into_bytes());
    Ok(Some(journal))
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!(
        "sha256:{}",
        digest.iter().fold(String::new(), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
    )
}

/// The text of `layers` in the session's first prompt build, hashed.
fn layers_hash(journal: &Journal, layers: &[&str]) -> Option<String> {
    let prompt = journal.prompts.first()?;
    let text: String = prompt
        .sections
        .iter()
        .filter(|(layer, _)| layers.contains(&layer.as_str()))
        .map(|(_, text)| text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.is_empty()).then(|| sha256(text.as_bytes()))
}

fn state(
    scope: &ProfileScope,
    store: &Store,
    row: &pmt_store::Session,
    journal: &Journal,
) -> Result<State, Error> {
    // The configuration the session ran with (the recording's header),
    // else the profile's.
    let header = journal.header.as_ref();
    let config = header
        .and_then(|h| h["config"].as_str())
        .and_then(|text| toml::from_str::<pmt_config::Config>(text).ok())
        .unwrap_or_else(|| scope.config().clone());
    let mut names = BTreeSet::new();
    super::trust::secret_names(
        &serde_json::to_value(&config).unwrap_or_default(),
        &mut names,
    );
    let toolsets: BTreeSet<String> = store
        .tool_surface(row.id)?
        .map(|surface| {
            surface
                .tools
                .into_iter()
                .filter(|t| t.exposure == "exposed")
                .map(|t| t.toolset)
                .collect()
        })
        .unwrap_or_default();
    let tool_schemas = store
        .session_prompt(row.id)?
        .map(|p| p.tools)
        .unwrap_or_default();
    Ok(State {
        source: row.source.as_str().to_owned(),
        source_tag: row.source_tag.clone(),
        provider: row.provider.clone(),
        model: row.model.clone(),
        cwd: row.cwd.clone(),
        config: toml::to_string(&config).unwrap_or_default(),
        secret_names: names.into_iter().collect(),
        toolsets: toolsets.into_iter().collect(),
        tool_schemas,
        skills_sha256: layers_hash(journal, &["skills_index", "pinned_skills"]),
        memory_sha256: layers_hash(journal, &["memory"]),
        harness_version: crate::build_info::VERSION.to_owned(),
        mock_script: header
            .and_then(|h| h["mock_script"].as_str())
            .map(str::to_owned),
        workspace_included: false,
    })
}

/// The workspace files the sessions' file tools changed, as they were
/// before the session (`None`: the file did not exist), from the file
/// snapshot journals (spec: execution-environments, File snapshots and
/// undo). Changes made otherwise (a terminal command) are not known.
fn before_session(
    scope: &ProfileScope,
    sessions: &[SessionId],
    root: &Path,
) -> BTreeMap<String, Option<Vec<u8>>> {
    let store = SnapshotStore::of_profile(scope.dir());
    let mut before = BTreeMap::new();
    let edits = sessions
        .iter()
        .flat_map(|session| journal::session_edits(&store, &session.to_string()))
        .filter(|edit| edit.backend.is_none());
    for edit in edits {
        let Ok(relative) = edit.path.strip_prefix(root) else {
            continue;
        };
        let name = relative_name(relative);
        if before.contains_key(&name) {
            continue;
        }
        if let Ok(content) = prior(&store, edit.before.as_deref()) {
            before.insert(name, content);
        }
    }
    before
}

/// A file's content before an edit (`None`: it did not exist); an error
/// when its snapshot is gone.
fn prior(store: &SnapshotStore, hash: Option<&str>) -> std::io::Result<Option<Vec<u8>>> {
    hash.map(|hash| store.get(hash)).transpose()
}

fn relative_name(relative: &Path) -> String {
    relative.to_string_lossy().replace('\\', "/")
}

/// The workspace as the session found it: the files the gitignore-aware
/// walk finds now, with the changes `before` knows of undone.
fn initial_files(
    root: &Path,
    before: &BTreeMap<String, Option<Vec<u8>>>,
) -> Result<(BTreeMap<String, Vec<u8>>, bool), Error> {
    let mut files = BTreeMap::new();
    let mut truncated = false;
    for entry in ignore::WalkBuilder::new(root).build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if files.len() == MAX_WORKSPACE_FILES {
            truncated = true;
            break;
        }
        let path = entry.path();
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let bytes = std::fs::read(path).map_err(|e| io_error(path, &e))?;
        files.insert(relative_name(relative), bytes);
    }
    for (name, content) in before {
        match content {
            Some(bytes) => files.insert(name.clone(), bytes.clone()),
            None => files.remove(name),
        };
    }
    Ok((files, truncated))
}

/// The workspace manifest (path, size, sha256 of each file as the session
/// found it) and, when `include`, the contents.
fn add_workspace(
    out: &mut Recording,
    root: Option<&Path>,
    before: &BTreeMap<String, Option<Vec<u8>>>,
    include: bool,
) -> Result<(), Error> {
    let (files, truncated) = match root.filter(|r| r.is_dir()) {
        Some(root) => initial_files(root, before)?,
        None => (BTreeMap::new(), false),
    };
    let entries: Vec<Value> = files
        .iter()
        .map(|(name, bytes)| json!({"path": name, "size": bytes.len(), "sha256": sha256(bytes)}))
        .collect();
    if include {
        for (name, bytes) in files {
            out.add(&member::workspace_file(&name), bytes);
        }
    }
    out.add_json(
        member::WORKSPACE,
        &json!({"root": root, "files": entries, "truncated": truncated}),
    );
    Ok(())
}

/// Applies the trajectory redaction to every member that is text.
fn redact(scope: &ProfileScope, out: &mut Recording) -> Result<(), Error> {
    let Some(redactor) = pmt_obs::redaction::profile_redactor(scope)? else {
        return Ok(());
    };
    out.map_members(|_, bytes| match String::from_utf8(bytes) {
        Ok(text) => redactor.redact_all(&text).into_bytes(),
        Err(err) => err.into_bytes(),
    });
    Ok(())
}
