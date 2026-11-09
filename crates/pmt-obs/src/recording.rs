//! The session recording journal (spec: replay-and-evaluation, Session
//! recordings): what a recorded session needs beyond its trajectory to be
//! replayed, one JSONL file per session at
//! `<profile>/recordings/<session_id>.jsonl`, written while it runs.
//!
//! Lines are `prompt` (each system prompt build, layer by layer), `call`
//! (each model call: request hash, canonical request, response chunks and
//! usage) and `tool` (each tool call's exact result). A canonical request
//! repeats its history on every call, so its system blocks, tool list and
//! each message are stored once as `object` lines and referenced as
//! `{"$obj": "<sha256>"}`; [`read`] puts them back. A session whose
//! trajectory is off is not recorded either, the file goes with the
//! trajectory under the same retention, and a write failure only logs.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use pmt_core::private_fs;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The recordings directory under a profile.
pub const DIR: &str = "recordings";

const REF: &str = "$obj";

/// The canonical request fields stored as objects.
const SHARED: [&str; 2] = ["system", "tools"];

/// One system prompt build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prompt {
    /// When it was built.
    pub ts: String,
    /// `(layer, text)` in prompt order.
    pub sections: Vec<(String, String)>,
}

/// One model call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Call {
    /// When it finished.
    pub ts: String,
    /// `sha256:<hex>` of the canonical request.
    pub request_hash: String,
    /// The canonical request.
    pub request: Value,
    /// The response: `{"chunks": [[event...]...], "usage": {...}}`.
    pub response: Value,
    /// Served from a replay or the cache rather than sent.
    #[serde(default)]
    pub served: bool,
}

/// One tool call and its result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    /// When it finished.
    pub ts: String,
    /// The model's call id.
    pub call_id: String,
    /// Tool name.
    pub name: String,
    /// The arguments as the model sent them.
    pub arguments: Value,
    /// The result the tool port returned.
    pub result: Value,
}

/// Everything recorded for one session, in order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Journal {
    /// What the recording process noted first (its mock script).
    pub header: Option<Value>,
    /// Prompt builds.
    pub prompts: Vec<Prompt>,
    /// Model calls.
    pub calls: Vec<Call>,
    /// Tool calls.
    pub tools: Vec<Tool>,
}

/// Appends to the recordings of one profile's sessions.
#[derive(Debug)]
pub struct Writer {
    dir: PathBuf,
    header: Option<Value>,
    /// Objects already in each session's file.
    known: Mutex<HashMap<String, HashSet<String>>>,
}

impl Writer {
    /// The writer of the profile at `profile_dir`; `header` is written
    /// first in each new file.
    #[must_use]
    pub fn new(profile_dir: &Path, header: Option<Value>) -> Self {
        Self {
            dir: profile_dir.join(DIR),
            header,
            known: Mutex::default(),
        }
    }

    /// Records a prompt build of `session`.
    pub fn prompt(&self, session: &str, prompt: &Prompt) {
        self.append(session, |_| line("prompt", prompt));
    }

    /// Records a model call of `session`.
    pub fn call(&self, session: &str, call: &Call) {
        self.append(session, |objects| {
            let mut call = call.clone();
            let request = std::mem::take(&mut call.request);
            call.request = share(request, objects);
            line("call", &call)
        });
    }

    /// Records a tool call of `session`.
    pub fn tool(&self, session: &str, tool: &Tool) {
        self.append(session, |_| line("tool", tool));
    }

    /// Appends `make`'s line, after the objects it introduced, to the
    /// session's file under one lock.
    fn append(&self, session: &str, make: impl FnOnce(&mut Vec<(String, Value)>) -> Value) {
        let path = self.dir.join(format!("{session}.jsonl"));
        let mut known = self.known.lock().unwrap_or_else(PoisonError::into_inner);
        let seen = known
            .entry(session.to_owned())
            .or_insert_with(|| objects_in(&path));
        let mut objects = Vec::new();
        let entry = make(&mut objects);
        let mut text = String::new();
        if !path.exists()
            && let Some(header) = &self.header
        {
            push_line(&mut text, &json!({"kind": "header", "value": header}));
        }
        for (id, value) in objects {
            if seen.insert(id.clone()) {
                push_line(
                    &mut text,
                    &json!({"kind": "object", "id": id, "value": value}),
                );
            }
        }
        push_line(&mut text, &entry);
        if let Err(err) = write(&self.dir, &path, &text) {
            tracing::warn!(error = %err, session, "session recording write failed");
        }
    }
}

fn line(kind: &str, entry: &impl Serialize) -> Value {
    let mut value = serde_json::to_value(entry).unwrap_or(Value::Null);
    if let Some(map) = value.as_object_mut() {
        map.insert("kind".to_owned(), kind.into());
    }
    value
}

fn push_line(text: &mut String, value: &Value) {
    text.push_str(&value.to_string());
    text.push('\n');
}

fn write(dir: &Path, path: &Path, text: &str) -> io::Result<()> {
    private_fs::create_dir(dir)?;
    private_fs::append_options()
        .open(path)?
        .write_all(text.as_bytes())
}

/// The object ids an existing file holds (a resumed session appends).
fn objects_in(path: &Path) -> HashSet<String> {
    let Ok(file) = fs::File::open(path) else {
        return HashSet::new();
    };
    io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
        .filter(|v| v["kind"] == "object")
        .filter_map(|v| v["id"].as_str().map(str::to_owned))
        .collect()
}

fn object_id(value: &Value) -> String {
    let digest = Sha256::digest(value.to_string());
    digest.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// `request` with its system blocks, tools and each message replaced by
/// object references (pushed to `objects`).
fn share(mut request: Value, objects: &mut Vec<(String, Value)>) -> Value {
    let mut refer = |value: Value| {
        let id = object_id(&value);
        objects.push((id.clone(), value));
        json!({ REF: id })
    };
    if let Some(map) = request.as_object_mut() {
        for key in SHARED {
            if let Some(value) = map.remove(key) {
                map.insert(key.to_owned(), refer(value));
            }
        }
        if let Some(Value::Array(messages)) = map.remove("messages") {
            let refs = messages.into_iter().map(&mut refer).collect();
            map.insert("messages".to_owned(), Value::Array(refs));
        }
    }
    request
}

/// `value` with every object reference replaced by its object.
fn resolve(value: Value, objects: &HashMap<String, Value>) -> io::Result<Value> {
    match value {
        Value::Object(map) if map.len() == 1 && map.contains_key(REF) => {
            let id = map[REF].as_str().unwrap_or_default();
            objects
                .get(id)
                .cloned()
                .ok_or_else(|| io::Error::other(format!("missing object {id}")))
        }
        Value::Object(map) => map
            .into_iter()
            .map(|(k, v)| resolve(v, objects).map(|v| (k, v)))
            .collect::<io::Result<_>>()
            .map(Value::Object),
        Value::Array(items) => items
            .into_iter()
            .map(|v| resolve(v, objects))
            .collect::<io::Result<_>>()
            .map(Value::Array),
        other => Ok(other),
    }
}

/// The recording file of `session` in the profile at `profile_dir`.
#[must_use]
pub fn path(profile_dir: &Path, session: &str) -> PathBuf {
    profile_dir.join(DIR).join(format!("{session}.jsonl"))
}

/// Reads the recording of `session`; `None` when it was not recorded.
///
/// # Errors
///
/// I/O errors, a malformed line or a dangling object reference.
pub fn read(profile_dir: &Path, session: &str) -> io::Result<Option<Journal>> {
    let file = match fs::File::open(path(profile_dir, session)) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let mut journal = Journal::default();
    let mut objects = HashMap::new();
    for text in io::BufReader::new(file).lines() {
        let mut value: Value = serde_json::from_str(&text?).map_err(io::Error::other)?;
        let kind = value["kind"].as_str().unwrap_or_default().to_owned();
        match kind.as_str() {
            "header" => journal.header = Some(value["value"].take()),
            "object" => {
                let id = value["id"].as_str().unwrap_or_default().to_owned();
                objects.insert(id, value["value"].take());
            }
            "prompt" => journal.prompts.push(parse(value)?),
            "call" => {
                let mut call: Call = parse(value)?;
                call.request = resolve(std::mem::take(&mut call.request), &objects)?;
                journal.calls.push(call);
            }
            "tool" => journal.tools.push(parse(value)?),
            _ => {}
        }
    }
    Ok(Some(journal))
}

fn parse<T: serde::de::DeserializeOwned>(value: Value) -> io::Result<T> {
    serde_json::from_value(value).map_err(io::Error::other)
}
