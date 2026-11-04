//! An MCP call result as the model sees it (spec: tool-system, MCP result
//! sanitization; MCP media results; Typed tool result contract).
//!
//! Text parts and embedded text resources are inlined (the dispatcher's
//! result budget bounds them) without TAG characters. Each image or audio
//! block, and each binary embedded resource, is stored by the host (the
//! profile's blob store) and reported as `MEDIA:<path> (<mime>)`; an image
//! is also attached as a media part when the session's route accepts
//! images. A single block larger than 50 MiB fails the result naming the
//! block. The result's `_meta` reaches the model as `meta`, without the
//! protocol-reserved keys.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use pmt_tools::result::Content;
use pmt_tools::{Status, ToolResult};
use rmcp::model::{CallToolResult, ContentBlock, ResourceContents};
use serde_json::{Map, Value};

use crate::host::ClientHost;
use crate::sanitize::{meta, strip_tags};

/// The largest single media block.
pub const MAX_BLOCK_BYTES: usize = 50 * 1024 * 1024;

/// The tool result of an MCP call result made for `session`.
#[must_use]
pub fn converted(result: &CallToolResult, host: &dyn ClientHost, session: &str) -> ToolResult {
    let mut out = Converted {
        host,
        session,
        content: Vec::new(),
    };
    for (index, block) in result.content.iter().enumerate() {
        if let Err(why) = out.block(index + 1, block) {
            return ToolResult::error(why);
        }
    }
    if out.content.is_empty()
        && let Some(structured) = &result.structured_content
    {
        out.text(crate::sanitize::value_without_tags(structured).to_string());
    }
    let mut metadata = Map::new();
    if let Some(given) = &result.meta {
        let kept = meta(given);
        if !kept.is_empty() {
            out.text(format!("meta: {}", Value::Object(kept.clone())));
            metadata.insert("meta".to_owned(), Value::Object(kept));
        }
    }
    let status = if result.is_error == Some(true) {
        Status::Error
    } else {
        Status::Ok
    };
    if out.content.is_empty() {
        out.text(String::new());
    }
    ToolResult {
        status,
        content: out.content,
        metadata,
    }
}

struct Converted<'a> {
    host: &'a dyn ClientHost,
    session: &'a str,
    content: Vec<Content>,
}

impl Converted<'_> {
    fn text(&mut self, text: String) {
        self.content.push(Content::Text { text });
    }

    fn block(&mut self, index: usize, block: &ContentBlock) -> Result<(), String> {
        match block {
            ContentBlock::Text(text) => self.text(strip_tags(&text.text)),
            ContentBlock::Image(image) => self.media(index, &image.data, &image.mime_type)?,
            ContentBlock::Audio(audio) => self.media(index, &audio.data, &audio.mime_type)?,
            ContentBlock::Resource(embedded) => match &embedded.resource {
                ResourceContents::TextResourceContents { text, .. } => self.text(strip_tags(text)),
                ResourceContents::BlobResourceContents {
                    blob, mime_type, ..
                } => self.media(
                    index,
                    blob,
                    mime_type.as_deref().unwrap_or("application/octet-stream"),
                )?,
                _ => self.text("[resource]".to_owned()),
            },
            ContentBlock::ResourceLink(link) => {
                self.text(format!("[resource link: {}]", strip_tags(&link.uri)));
            }
            _ => self.text("[content]".to_owned()),
        }
        Ok(())
    }

    /// Stores one base64 media block and reports it.
    fn media(&mut self, index: usize, data: &str, mime: &str) -> Result<(), String> {
        let mime = strip_tags(mime);
        if data.len() / 4 * 3 > MAX_BLOCK_BYTES {
            return Err(format!(
                "content block {index} ({mime}) is larger than 50 MiB"
            ));
        }
        let bytes = match STANDARD.decode(data.trim()) {
            Ok(bytes) => bytes,
            Err(err) => {
                self.text(format!(
                    "[content block {index} ({mime}): invalid base64: {err}]"
                ));
                return Ok(());
            }
        };
        if bytes.len() > MAX_BLOCK_BYTES {
            return Err(format!(
                "content block {index} ({mime}) is larger than 50 MiB"
            ));
        }
        match self.host.store_media(self.session, &bytes, &mime) {
            Ok(path) => {
                self.text(format!("MEDIA:{path} ({mime})"));
                if mime.starts_with("image/") && self.host.accepts_images(self.session) {
                    self.content.push(Content::Media { mime, path });
                }
            }
            Err(why) => self.text(format!(
                "[content block {index} ({mime}, {} bytes) not stored: {why}]",
                bytes.len()
            )),
        }
        Ok(())
    }
}
