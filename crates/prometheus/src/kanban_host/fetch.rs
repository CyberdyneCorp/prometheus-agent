//! `kanban_attach_url` fetches (spec: kanban, URL attachments and
//! attachment listing tools; security-boundaries, In-process fetch SSRF
//! protection). Only `http` and `https` URLs; every hop passes the
//! profile's egress host policy and destination policy (private, loopback,
//! link-local and metadata addresses refused unless
//! `security.egress.allow_private`), over its proxy; the body streams
//! under the attachment cap and a larger one is abandoned with nothing
//! stored.

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_net::fetch::Fetcher;

/// Who the egress audit names for these fetches.
const REQUESTER: &str = "kanban_attach_url";

/// A fetched attachment.
pub struct Fetched {
    /// The body.
    pub bytes: Vec<u8>,
    /// The URL path's last segment (`attachment` when it has none).
    pub filename: String,
    /// The response's `Content-Type`.
    pub content_type: Option<String>,
}

/// The fetcher of `scope`'s URL attachments.
///
/// # Errors
///
/// Invalid egress settings, an unbuildable client.
pub fn fetcher(scope: &ProfileScope) -> Result<Fetcher, Error> {
    crate::egress::fetcher(scope, REQUESTER, None)
}

fn filename(url: &reqwest::Url) -> String {
    url.path_segments()
        .and_then(|mut segments| segments.next_back().map(str::to_owned))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "attachment".to_owned())
}

/// Fetches `url`, at most `limit` bytes.
///
/// # Errors
///
/// `kanban.invalid_task` for a URL that is not `http(s)`, the egress
/// refusal, `kanban.attachment_too_large`, a failed response.
pub async fn get(fetcher: &Fetcher, url: &str, limit: u64) -> Result<Fetched, Error> {
    let parsed = reqwest::Url::parse(url).map_err(|e| {
        Error::new(
            &codes::KANBAN_INVALID_TASK,
            format!("invalid url '{url}': {e}"),
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(Error::new(
            &codes::KANBAN_INVALID_TASK,
            format!(
                "only http and https URLs can be attached (not {})",
                parsed.scheme()
            ),
        ));
    }
    let mut response = fetcher.get(url).await?;
    if !response.status().is_success() {
        return Err(Error::new(
            &codes::KANBAN_INVALID_TASK,
            format!("{url} answered HTTP {}", response.status().as_u16()),
        ));
    }
    if let Some(size) = response.content_length().filter(|size| *size > limit) {
        return Err(pmt_kanban::attach::too_large(size, limit));
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_owned());
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| Error::new(&codes::KANBAN_INVALID_TASK, format!("{url}: {e}")))?
    {
        bytes.extend_from_slice(&chunk);
        let size = bytes.len() as u64;
        if size > limit {
            return Err(pmt_kanban::attach::too_large(size, limit));
        }
    }
    Ok(Fetched {
        bytes,
        filename: filename(&parsed),
        content_type,
    })
}
