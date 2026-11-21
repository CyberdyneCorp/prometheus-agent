//! What crosses the plugin boundary besides text for a WASM adapter
//! (spec: platform-adapters, WASM adapter plugin isolation; Inbound media
//! limits and caching): files carried both ways as base64 inside the JSON
//! forms, and the poll-error prefix that stops an adapter for good.
//!
//! A plugin cannot read the daemon's files nor hand the gateway a stream,
//! so an outgoing attachment travels as its bytes (`attachments` of the
//! send JSON, with its local `path` for a platform daemon on this machine
//! that sends by path) and an inbound one as bytes the plugin downloaded
//! under its own network grants (`data`), as a local file a platform
//! daemon saved (`path`, read here only inside the plugin's `fs_read`
//! grant), both kept until the gateway opens them through its size cap
//! and cache, or as a bare platform reference the gateway passes on as it
//! is (`location`).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use base64::Engine as _;
use pmt_core::capability::Capability;
use pmt_gateway::messaging::adapter::OutboundAttachment;
use pmt_gateway::messaging::conformance::{kind_of_name, mime_of_name};
use pmt_gateway::messaging::{
    AdapterError, AdapterState, Attachment, MediaKind, MediaStream, SendError,
};
use pmt_plugins::grants::PluginGrants;
use serde_json::{Value, json};

/// The largest file carried across the boundary in either direction.
pub(super) const MAX_CARRIED_BYTES: usize = 25 * 1024 * 1024;
/// Inbound files kept for the gateway at once (it opens each right after
/// the poll that brought it; older ones are dropped).
const KEPT: usize = 64;
/// The location prefix of a file the plugin handed over as bytes.
const CARRIED: &str = "plugin-media:";

/// `attachments` for the send JSON: each file's name, kind, MIME type,
/// delivery hints and bytes.
pub(super) async fn outgoing(attachments: &[OutboundAttachment]) -> Result<Vec<Value>, SendError> {
    let mut carried = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        let path = std::path::Path::new(&attachment.path);
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|e| SendError::Permanent(format!("cannot read {}: {e}", attachment.path)))?;
        if bytes.len() > MAX_CARRIED_BYTES {
            return Err(SendError::Permanent(format!(
                "{} is larger than the {} MiB an adapter plugin may be handed",
                attachment.path,
                MAX_CARRIED_BYTES / (1024 * 1024)
            )));
        }
        let name = path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let kind = if attachment.as_voice {
            MediaKind::Voice
        } else if attachment.as_document {
            MediaKind::Document
        } else {
            kind_of_name(&name)
        };
        carried.push(json!({
            "path": attachment.path,
            "name": name,
            "kind": kind,
            "mime": mime_of_name(&name),
            "as_document": attachment.as_document,
            "as_voice": attachment.as_voice,
            "data": base64::engine::general_purpose::STANDARD.encode(bytes),
        }));
    }
    Ok(carried)
}

/// Inbound files a plugin handed over, until the gateway opens them.
#[derive(Default)]
pub(super) struct Inbox {
    kept: Mutex<Kept>,
    /// The plugin's grants, which bound the local files it may name.
    grants: Option<Arc<PluginGrants>>,
}

/// The files held, by location, and the last location number handed out.
#[derive(Default)]
struct Kept {
    last: u64,
    files: VecDeque<(String, Vec<u8>)>,
}

impl Inbox {
    /// An inbox for a plugin holding `grants`.
    pub(super) fn granted(grants: Arc<PluginGrants>) -> Self {
        Self {
            kept: Mutex::default(),
            grants: Some(grants),
        }
    }

    /// The bytes of the local file `path`, when the plugin's grants let it
    /// read it (resolved first, so a link cannot lead outside the grant).
    fn read_granted(&self, path: &str) -> Option<Vec<u8>> {
        let resolved = std::fs::canonicalize(path).ok()?;
        let wanted = Capability::parse(&format!("fs.read:{}", resolved.display())).ok()?;
        let granted = self.grants.as_ref()?.current().allows(&wanted);
        if !granted {
            tracing::warn!(
                path = %resolved.display(),
                "an adapter plugin named a file outside its fs_read grant; the attachment is dropped"
            );
            return None;
        }
        let bytes = std::fs::read(&resolved).ok()?;
        (bytes.len() <= MAX_CARRIED_BYTES).then_some(bytes)
    }

    fn keep(&self, bytes: Vec<u8>) -> String {
        let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        kept.last += 1;
        let location = format!("{CARRIED}{}", kept.last);
        kept.files.push_back((location.clone(), bytes));
        while kept.files.len() > KEPT {
            kept.files.pop_front();
        }
        location
    }

    /// The attachments of one polled message (`attachments: [{kind,
    /// name?, location?, data?, duration_secs?}]`); one with neither
    /// bytes nor location, or an unknown kind, is skipped.
    pub(super) fn attachments(&self, message: &Value) -> Vec<Attachment> {
        let mut out = Vec::new();
        for item in message["attachments"].as_array().into_iter().flatten() {
            let Ok(kind) = serde_json::from_value::<MediaKind>(item["kind"].clone()) else {
                continue;
            };
            let data = item["data"].as_str().and_then(|d| {
                base64::engine::general_purpose::STANDARD
                    .decode(d)
                    .ok()
                    .filter(|b| b.len() <= MAX_CARRIED_BYTES)
            });
            let data = data.or_else(|| item["path"].as_str().and_then(|p| self.read_granted(p)));
            let location = match (data, item["location"].as_str()) {
                (Some(bytes), _) => self.keep(bytes),
                (None, Some(location)) if !location.is_empty() => location.to_owned(),
                _ => continue,
            };
            let mut attachment = Attachment::new(kind, &location);
            attachment.name = item["name"].as_str().map(str::to_owned);
            attachment.duration_secs = item["duration_secs"].as_u64();
            out.push(attachment);
        }
        out
    }

    /// The bytes of a handed-over file, once: `unsupported` for a
    /// platform reference (the gateway then uses it as it is).
    pub(super) fn open(&self, attachment: &Attachment) -> Result<MediaStream, SendError> {
        if !attachment.location.starts_with(CARRIED) {
            return Err(SendError::Unsupported);
        }
        let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        let at = kept
            .files
            .iter()
            .position(|(l, _)| *l == attachment.location)
            .ok_or_else(|| SendError::Permanent("the attachment is no longer held".to_owned()))?;
        let (_, bytes) = kept.files.remove(at).unwrap_or_default();
        Ok(MediaStream {
            declared_len: Some(bytes.len() as u64),
            chunks: Box::pin(futures_util::stream::iter([Ok(bytes)])),
        })
    }
}

/// The state a failed poll puts the adapter in: `fatal: <code>: <why>`
/// stops it until it is reconfigured (a revoked credential, a topic that
/// does not exist), anything else is retried with backoff.
pub(super) fn poll_failure(why: String) -> (AdapterState, AdapterError) {
    if let Some(rest) = why.strip_prefix("fatal:") {
        let (code, message) = rest.trim().split_once(':').unwrap_or((rest.trim(), ""));
        let code = code.trim();
        if !code.is_empty() && !code.contains(char::is_whitespace) {
            return (
                AdapterState::Fatal,
                AdapterError {
                    retryable: false,
                    code: code.to_owned(),
                    message: message.trim().to_owned(),
                },
            );
        }
    }
    (
        AdapterState::Retrying,
        AdapterError {
            retryable: true,
            code: "plugin_poll".to_owned(),
            message: why,
        },
    )
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt as _;

    use super::*;

    #[test]
    fn a_fatal_prefix_names_the_code_and_stops_for_good() {
        let (state, error) = poll_failure("fatal: ntfy_unauthorized: 401 from ntfy.sh".to_owned());
        assert_eq!(state, AdapterState::Fatal);
        assert_eq!(error.code, "ntfy_unauthorized");
        assert_eq!(error.message, "401 from ntfy.sh");
        assert!(!error.retryable);
        let (state, error) = poll_failure("connection reset".to_owned());
        assert_eq!(state, AdapterState::Retrying);
        assert_eq!(error.code, "plugin_poll");
        // A sentence that merely starts with the word is not a code.
        assert_eq!(
            poll_failure("fatal: the server said no".to_owned()).0,
            AdapterState::Retrying
        );
    }

    #[tokio::test]
    async fn handed_over_bytes_open_once_and_references_pass_through() {
        let inbox = Inbox::default();
        let message = json!({"attachments": [
            {"kind": "image", "name": "a.png", "data": "aGVsbG8="},
            {"kind": "document", "location": "https://cdn.example/f"},
            {"kind": "nonsense", "location": "x"},
            {"kind": "voice"},
        ]});
        let attachments = inbox.attachments(&message);
        assert_eq!(attachments.len(), 2);
        assert_eq!(attachments[0].name.as_deref(), Some("a.png"));
        let mut stream = inbox.open(&attachments[0]).unwrap();
        assert_eq!(stream.declared_len, Some(5));
        assert_eq!(stream.chunks.next().await.unwrap().unwrap(), b"hello");
        assert!(inbox.open(&attachments[0]).is_err());
        assert_eq!(
            inbox.open(&attachments[1]).unwrap_err(),
            SendError::Unsupported
        );
    }
}
