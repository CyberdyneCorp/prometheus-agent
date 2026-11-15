//! ACP prompt content to a daemon prompt (spec: acp-adapter, Prompt
//! content conversion and media staging): text stays text, file
//! `resource_link`s are inlined up to 512 KiB (larger files truncated with
//! a note, binary files omitted with a note), images are sent as their
//! bytes for the daemon to stage in the profile's content-addressed store
//! once their type is sniffed from the bytes (PNG, JPEG, GIF and WebP
//! only), and remote image URLs are fetched only through the
//! egress-controlled fetcher, falling back to a text mention. A prompt
//! carries at most 10 attachments.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use pmt_rpc::contracts::PromptImage;
use serde_json::Value;

use crate::frames::AcpError;

/// The most attachments one prompt may carry.
pub(crate) const MAX_ATTACHMENTS: usize = 10;

/// The most bytes of a file inlined into a prompt.
pub(crate) const MAX_INLINE: usize = 512 * 1024;

/// What an unsupported image becomes.
pub(crate) const UNSUPPORTED_IMAGE: &str = "[Image omitted: unsupported image format]";

/// Fetches a remote image under the profile's egress policy; `None` when
/// it cannot (refused, unreachable, too large).
pub trait Fetcher: Send + Sync {
    /// The bytes behind `url`.
    fn fetch<'a>(
        &'a self,
        url: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = Option<Vec<u8>>> + Send + 'a>>;
}

/// A converted prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Converted {
    /// The prompt text, notes included.
    pub(crate) text: String,
    /// The images, as `data:` URLs the daemon stages.
    pub(crate) images: Vec<PromptImage>,
}

/// The image type of `bytes`, sniffed from their signature.
pub(crate) fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// How many attachments `blocks` carry (everything but text).
fn attachments(blocks: &[Value]) -> usize {
    blocks
        .iter()
        .filter(|b| b["type"].as_str() != Some("text"))
        .count()
}

/// The local path of a `file://` URI.
fn file_path(uri: &str) -> Option<PathBuf> {
    url::Url::parse(uri).ok()?.to_file_path().ok()
}

/// A file's text as a prompt shows it, with the note its size or kind
/// calls for.
pub(crate) fn inline_file(path: &Path, bytes: &[u8]) -> String {
    let shown = path.display();
    if bytes.contains(&0) || std::str::from_utf8(&bytes[..bytes.len().min(MAX_INLINE)]).is_err() {
        return format!("[File omitted: {shown} is a binary file]");
    }
    if bytes.len() <= MAX_INLINE {
        let text = String::from_utf8_lossy(bytes);
        return format!("<file path=\"{shown}\">\n{text}\n</file>");
    }
    let text = String::from_utf8_lossy(&bytes[..MAX_INLINE]);
    format!(
        "<file path=\"{shown}\">\n{text}\n</file>\n[File truncated: {shown} is {} bytes; only the first 512 KiB is included]",
        bytes.len()
    )
}

impl Converted {
    fn note(&mut self, text: &str) {
        if !self.text.is_empty() {
            self.text.push_str("\n\n");
        }
        self.text.push_str(text);
    }

    fn image(&mut self, bytes: &[u8]) {
        match sniff(bytes) {
            Some(mime) => self.images.push(PromptImage::Data(format!(
                "data:{mime};base64,{}",
                STANDARD.encode(bytes)
            ))),
            None => self.note(UNSUPPORTED_IMAGE),
        }
    }
}

/// Converts the blocks of a `session/prompt`.
///
/// # Errors
///
/// `-32602 too many attachments (max 10)`.
pub(crate) async fn convert(
    blocks: &[Value],
    fetcher: &dyn Fetcher,
) -> Result<Converted, AcpError> {
    if attachments(blocks) > MAX_ATTACHMENTS {
        return Err(AcpError::invalid(format!(
            "too many attachments (max {MAX_ATTACHMENTS})"
        )));
    }
    let mut out = Converted::default();
    for block in blocks {
        match block["type"].as_str().unwrap_or_default() {
            "text" => out.note(block["text"].as_str().unwrap_or_default()),
            "image" => image_block(&mut out, block, fetcher).await,
            "resource_link" => link_block(&mut out, block),
            "resource" => resource_block(&mut out, &block["resource"]),
            other => out.note(&format!("[{other} content omitted: not supported]")),
        }
    }
    Ok(out)
}

async fn image_block(out: &mut Converted, block: &Value, fetcher: &dyn Fetcher) {
    if let Some(data) = block["data"].as_str().filter(|d| !d.is_empty()) {
        match STANDARD.decode(data) {
            Ok(bytes) => out.image(&bytes),
            Err(_) => out.note(UNSUPPORTED_IMAGE),
        }
        return;
    }
    let Some(url) = block["uri"].as_str() else {
        out.note(UNSUPPORTED_IMAGE);
        return;
    };
    let remote = url.starts_with("https://") || url.starts_with("http://");
    match (remote, file_path(url)) {
        (true, _) => match fetcher.fetch(url).await {
            Some(bytes) if sniff(&bytes).is_some() => out.image(&bytes),
            _ => out.note(&format!("[Image attached: {url}]")),
        },
        (false, Some(path)) => match std::fs::read(&path) {
            Ok(bytes) => out.image(&bytes),
            Err(_) => out.note(&format!("[Image attached: {url}]")),
        },
        (false, None) => out.note(&format!("[Image attached: {url}]")),
    }
}

fn link_block(out: &mut Converted, block: &Value) {
    let uri = block["uri"].as_str().unwrap_or_default();
    let Some(path) = file_path(uri) else {
        out.note(&format!("[Resource: {uri}]"));
        return;
    };
    match std::fs::read(&path) {
        Ok(bytes) => out.note(&inline_file(&path, &bytes)),
        Err(err) => out.note(&format!(
            "[File omitted: {} cannot be read: {err}]",
            path.display()
        )),
    }
}

fn resource_block(out: &mut Converted, resource: &Value) {
    let uri = resource["uri"].as_str().unwrap_or_default();
    if let Some(text) = resource["text"].as_str() {
        let path = file_path(uri).unwrap_or_else(|| PathBuf::from(uri));
        out.note(&inline_file(&path, text.as_bytes()));
        return;
    }
    let blob = resource["blob"]
        .as_str()
        .and_then(|b| STANDARD.decode(b).ok());
    match blob {
        Some(bytes) if sniff(&bytes).is_some() => out.image(&bytes),
        _ => out.note(&format!("[File omitted: {uri} is a binary file]")),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Converted, Fetcher, MAX_INLINE, UNSUPPORTED_IMAGE, convert, inline_file, sniff};

    struct NoNetwork;

    impl Fetcher for NoNetwork {
        fn fetch<'a>(
            &'a self,
            _: &'a str,
        ) -> std::pin::Pin<Box<dyn Future<Output = Option<Vec<u8>>> + Send + 'a>> {
            Box::pin(async { None })
        }
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    async fn converted(blocks: serde_json::Value) -> Converted {
        convert(blocks.as_array().unwrap(), &NoNetwork)
            .await
            .unwrap()
    }

    /// Scenario: Unsupported image type.
    #[tokio::test]
    async fn an_svg_is_omitted_with_a_note_and_the_text_still_runs() {
        use base64::Engine as _;
        let svg = base64::engine::general_purpose::STANDARD.encode(b"<svg xmlns='x'></svg>");
        let out = converted(json!([
            {"type": "text", "text": "what is this?"},
            {"type": "image", "mimeType": "image/svg+xml", "data": svg}
        ]))
        .await;
        assert!(out.images.is_empty());
        assert!(out.text.contains("what is this?"));
        assert!(out.text.contains(UNSUPPORTED_IMAGE));
    }

    #[tokio::test]
    async fn images_are_typed_from_their_bytes_and_remote_ones_fall_back_to_a_mention() {
        use base64::Engine as _;
        let png = base64::engine::general_purpose::STANDARD.encode(PNG);
        let out = converted(json!([
            {"type": "image", "mimeType": "image/jpeg", "data": png},
            {"type": "image", "uri": "https://example.invalid/cat.png"}
        ]))
        .await;
        assert_eq!(out.images.len(), 1);
        assert!(
            matches!(&out.images[0], pmt_rpc::contracts::PromptImage::Data(d) if d.starts_with("data:image/png;base64,"))
        );
        assert!(
            out.text
                .contains("[Image attached: https://example.invalid/cat.png]")
        );
    }

    #[tokio::test]
    async fn eleven_attachments_are_refused() {
        let blocks: Vec<_> = (0..11)
            .map(|n| json!({"type": "resource_link", "uri": format!("file:///tmp/{n}"), "name": "f"}))
            .collect();
        let err = convert(&blocks, &NoNetwork).await.unwrap_err();
        assert_eq!(err.code, -32602);
        assert_eq!(err.message, "too many attachments (max 10)");
    }

    #[test]
    fn files_are_inlined_truncated_or_omitted() {
        let path = std::path::Path::new("/work/a.txt");
        assert!(inline_file(path, b"hello").contains("hello"));
        let big = vec![b'x'; MAX_INLINE + 10];
        let shown = inline_file(path, &big);
        assert!(shown.contains("[File truncated: /work/a.txt is"));
        assert!(inline_file(path, b"\0\x01binary").starts_with("[File omitted:"));
        assert_eq!(sniff(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
    }
}
