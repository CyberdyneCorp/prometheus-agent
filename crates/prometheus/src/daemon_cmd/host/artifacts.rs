//! `artifacts.list` (spec: desktop-app, Artifacts gallery and
//! suggestions): the images, files and links the profile's sessions
//! produced, newest first, read from the stored transcripts: a file a
//! successful write (`write_file`, `patch`) created, an image path a
//! successful tool result names and a link an answer gives. Failed calls
//! and read-only inspection add nothing.

use std::sync::LazyLock;

use pmt_core::error::Error;
use pmt_rpc::contracts::{Artifact, ArtifactList, ArtifactsListParams};
use pmt_store::{ListOptions, Message, Visibility};
use regex::Regex;
use serde_json::Value;

use super::{Profile, session_id};

/// Entries per page.
const PAGE: usize = 50;
/// The most recent sessions searched.
const SESSIONS: u32 = 500;

/// Tools whose successful call writes the file named by `path`.
const WRITERS: &[&str] = &["write_file", "patch"];

static IMAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:/|\./|~/)?[\w@~./-]*[\w-]\.(?:png|jpe?g|gif|webp|svg|bmp)\b")
        .expect("valid regex")
});
static LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"https?://[^\s<>"')\]]+"#).expect("valid regex"));

/// A page of `profile`'s artifacts.
pub(super) fn list(profile: &Profile, params: &ArtifactsListParams) -> Result<ArtifactList, Error> {
    let sessions = match &params.session_id {
        Some(id) => vec![profile.store.session(session_id(id)?)?],
        None => profile.store.list_sessions(&ListOptions {
            limit: Some(SESSIONS),
            ..ListOptions::default()
        })?,
    };
    let query = params.query.as_deref().unwrap_or("").to_lowercase();
    let mut found: Vec<Artifact> = Vec::new();
    for session in sessions {
        let title = session.title.clone().unwrap_or_default();
        let messages = profile.store.messages(session.id, Visibility::Active)?;
        for artifact in of_session(&session.id.to_string(), &title, &messages) {
            let haystack = format!("{} {}", artifact.value, artifact.session_title).to_lowercase();
            if haystack.contains(&query) {
                found.push(artifact);
            }
        }
    }
    found.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
    let start = params
        .cursor
        .as_deref()
        .and_then(|c| c.parse::<usize>().ok())
        .unwrap_or(0);
    let end = (start + PAGE).min(found.len());
    let next_cursor = (end < found.len()).then(|| end.to_string());
    Ok(ArtifactList {
        artifacts: found.get(start..end).unwrap_or_default().to_vec(),
        next_cursor,
    })
}

/// What a message produced: kind, value and the call behind it.
type Found = (&'static str, String, Option<(String, String)>);

/// One session's artifacts, oldest first.
pub(super) fn of_session(session: &str, title: &str, messages: &[Message]) -> Vec<Artifact> {
    let mut calls: Vec<(String, String, Value)> = Vec::new();
    let mut out = Vec::new();
    for message in messages {
        let found = match message.role.as_str() {
            "assistant" => {
                calls.extend(tool_calls(message.tool_calls.as_ref()));
                links(message)
            }
            "tool" if succeeded(message) => from_tool(message, &calls),
            _ => Vec::new(),
        };
        for (kind, value, call) in found {
            out.push(Artifact {
                id: format!("{}:{}", message.id, out.len()),
                kind: kind.to_owned(),
                value,
                session_id: session.to_owned(),
                session_title: title.to_owned(),
                created_at: message.created_at.to_string(),
                tool: call.as_ref().map(|(_, tool)| tool.clone()),
                call_id: call.map(|(id, _)| id),
            });
        }
    }
    out
}

fn links(message: &Message) -> Vec<Found> {
    LINK.find_iter(message.content.as_deref().unwrap_or_default())
        .map(|link| {
            (
                "link",
                link.as_str().trim_end_matches(['.', ',']).to_owned(),
                None,
            )
        })
        .collect()
}

/// A successful call's artifacts: the file a writer wrote, else the
/// images its output names.
fn from_tool(message: &Message, calls: &[(String, String, Value)]) -> Vec<Found> {
    let call_id = message.tool_call_id.clone().unwrap_or_default();
    let Some((_, tool, args)) = calls.iter().find(|(id, _, _)| *id == call_id) else {
        return Vec::new();
    };
    let call = Some((call_id.clone(), tool.clone()));
    if WRITERS.contains(&tool.as_str()) {
        return args
            .get("path")
            .and_then(Value::as_str)
            .map(|path| vec![(kind_of(path), path.to_owned(), call)])
            .unwrap_or_default();
    }
    IMAGE
        .find_iter(message.content.as_deref().unwrap_or_default())
        .map(|image| ("image", image.as_str().to_owned(), call.clone()))
        .collect()
}

fn kind_of(path: &str) -> &'static str {
    if IMAGE.is_match(path) {
        "image"
    } else {
        "file"
    }
}

fn succeeded(message: &Message) -> bool {
    !message
        .metadata
        .as_ref()
        .and_then(|m| m.get("is_error"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// `(call id, tool, arguments)` of an assistant message's calls.
fn tool_calls(calls: Option<&Value>) -> Vec<(String, String, Value)> {
    let Some(Value::Array(calls)) = calls else {
        return Vec::new();
    };
    calls
        .iter()
        .filter_map(|call| {
            let id = call.get("id")?.as_str()?.to_owned();
            let function = call.get("function").unwrap_or(call);
            let name = function.get("name")?.as_str()?.to_owned();
            let args = match function.get("arguments") {
                Some(Value::String(text)) => serde_json::from_str(text).unwrap_or(Value::Null),
                Some(other) => other.clone(),
                None => Value::Null,
            };
            Some((id, name, args))
        })
        .collect()
}
