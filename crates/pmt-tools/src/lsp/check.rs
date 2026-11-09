//! One check against a running server: the diagnostics of a document's
//! text, and the workspace diagnostics of `lsp.wait_mode = "full"` (spec:
//! lsp-diagnostics, Diagnostics introduced by an edit; Wait modes and
//! timeouts).

use std::collections::HashMap;
use std::hash::BuildHasher;

use serde_json::json;
use tokio::time::Instant;

use super::pool::{Document, Running};
use super::protocol::{self, Diagnostic};

/// The server did not publish in time (or exited).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedOut;

/// The diagnostics of `text` as the content of `uri`: sends it (opening
/// the document the first time) and waits for the verdict on that
/// version. Text the server already has answers from the last verdict.
///
/// # Errors
///
/// No verdict before `deadline`.
pub async fn diagnostics(
    server: &Running,
    uri: &str,
    language: &str,
    text: &str,
    deadline: Instant,
) -> Result<Vec<Diagnostic>, TimedOut> {
    let mut documents = server.documents.lock().await;
    if let Some(document) = documents.get(uri).filter(|d| d.text == text) {
        return Ok(document.diagnostics.clone());
    }
    let after_seq = server.client.seq();
    let version = if let Some(document) = documents.get(uri) {
        let version = document.version + 1;
        let params = json!({
            "textDocument": {"uri": uri, "version": version},
            "contentChanges": [{"text": text}],
        });
        server
            .client
            .notify("textDocument/didChange", params)
            .await
            .map_err(|_| TimedOut)?;
        version
    } else {
        let params = json!({"textDocument": {
            "uri": uri, "languageId": language, "version": 1, "text": text,
        }});
        server
            .client
            .notify("textDocument/didOpen", params)
            .await
            .map_err(|_| TimedOut)?;
        1
    };
    // Recorded before waiting: a timed-out version is still what the
    // server has, so the next check sends a newer one.
    documents.insert(
        uri.to_owned(),
        Document {
            version,
            text: String::new(),
            diagnostics: Vec::new(),
        },
    );
    let published = server
        .client
        .diagnostics(uri, version, after_seq, deadline)
        .await
        .ok_or(TimedOut)?;
    documents.insert(
        uri.to_owned(),
        Document {
            version,
            text: text.to_owned(),
            diagnostics: published.clone(),
        },
    );
    Ok(published)
}

/// Every other document's diagnostics now: the server's
/// `workspace/diagnostic` answer when it has one, else what it published.
pub async fn workspace(server: &Running, deadline: Instant) -> HashMap<String, Vec<Diagnostic>> {
    if server.client.has_workspace_diagnostics()
        && let Ok(reports) = server.client.workspace_diagnostics(deadline).await
    {
        return reports;
    }
    server.client.published()
}

/// The diagnostics of other documents that `after` has and `before` did
/// not, as `<path>: <rendered>` lines.
#[must_use]
pub fn introduced_elsewhere<S: BuildHasher, T: BuildHasher>(
    edited: &str,
    before: &HashMap<String, Vec<Diagnostic>, S>,
    after: &HashMap<String, Vec<Diagnostic>, T>,
    server: &str,
) -> Vec<String> {
    let mut uris: Vec<&String> = after.keys().filter(|uri| uri.as_str() != edited).collect();
    uris.sort();
    uris.into_iter()
        .flat_map(|uri| {
            let old = before.get(uri).map_or(&[][..], Vec::as_slice);
            let path = url::Url::parse(uri)
                .ok()
                .and_then(|u| u.to_file_path().ok())
                .map_or_else(|| uri.clone(), |p| p.display().to_string());
            protocol::introduced(old, &after[uri])
                .into_iter()
                .map(move |d| format!("{path}: {}", d.render(server)))
        })
        .collect()
}
