//! HTTP plumbing shared by the media providers: JSON and binary replies
//! with the status in the error (never the URL, which may carry a key),
//! capped downloads, `data:` URLs and `multipart/form-data` bodies.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::CONTENT_TYPE;
use serde_json::Value;

/// A transport error without its URL.
#[must_use]
pub fn transport(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        "timed out".to_owned()
    } else if err.is_connect() {
        "connection failed".to_owned()
    } else {
        "request failed".to_owned()
    }
}

/// A reply's body when its status is a success.
///
/// # Errors
///
/// `HTTP <status>: <message>` (the provider's error message when the body
/// carries one), or the transport failure.
pub async fn ok_bytes(
    sent: reqwest::Result<reqwest::Response>,
) -> Result<(Vec<u8>, String), String> {
    let response = sent.map_err(|e| transport(&e))?;
    let status = response.status();
    let mime = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = response.bytes().await.map_err(|e| transport(&e))?;
    if !status.is_success() {
        return Err(status_error(status.as_u16(), &body));
    }
    Ok((body.to_vec(), mime))
}

/// A reply's JSON when its status is a success.
///
/// # Errors
///
/// As [`ok_bytes`], or an unreadable body.
pub async fn ok_json(sent: reqwest::Result<reqwest::Response>) -> Result<Value, String> {
    let (body, _) = ok_bytes(sent).await?;
    serde_json::from_slice(&body).map_err(|e| format!("unreadable reply: {e}"))
}

/// `HTTP <status>`, with the provider's own message when it gave one.
#[must_use]
pub fn status_error(status: u16, body: &[u8]) -> String {
    let message = serde_json::from_slice::<Value>(body).ok().and_then(|v| {
        [
            "/error/message",
            "/error",
            "/message",
            "/detail",
            "/detail/message",
        ]
        .iter()
        .find_map(|p| v.pointer(p).and_then(Value::as_str).map(str::to_owned))
    });
    match message {
        Some(message) => format!(
            "HTTP {status}: {}",
            message.chars().take(300).collect::<String>()
        ),
        None => format!("HTTP {status}"),
    }
}

/// Decodes a `data:<mime>;base64,<data>` URL.
#[must_use]
pub fn data_url(url: &str) -> Option<(Vec<u8>, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let mime = meta.strip_suffix(";base64")?;
    let bytes = STANDARD.decode(data.trim()).ok()?;
    Some((bytes, mime.to_owned()))
}

/// Downloads `url` (a `data:` URL is decoded), refusing more than `cap`
/// bytes.
///
/// # Errors
///
/// HTTP and transport failures; a body over `cap`.
pub async fn download(
    client: &reqwest::Client,
    url: &str,
    cap: u64,
) -> Result<(Vec<u8>, String), String> {
    if let Some(decoded) = data_url(url) {
        return Ok(decoded);
    }
    let response = client.get(url).send().await.map_err(|e| transport(&e))?;
    read_capped(response, cap).await
}

/// The body of `response` when it succeeded, refusing more than `cap`
/// bytes (`larger than <n> MiB`).
///
/// # Errors
///
/// A failed status, a transport failure or a body over `cap`.
pub async fn read_capped(
    mut response: reqwest::Response,
    cap: u64,
) -> Result<(Vec<u8>, String), String> {
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    let mime = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    if response.content_length().is_some_and(|n| n > cap) {
        return Err(format!("larger than {} MiB", cap >> 20));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| transport(&e))? {
        body.extend_from_slice(&chunk);
        if body.len() as u64 > cap {
            return Err(format!("larger than {} MiB", cap >> 20));
        }
    }
    Ok((body, mime))
}

/// A `multipart/form-data` body.
pub struct Multipart {
    boundary: String,
    body: Vec<u8>,
}

impl Default for Multipart {
    fn default() -> Self {
        Self::new()
    }
}

impl Multipart {
    /// An empty body with a random boundary.
    #[must_use]
    pub fn new() -> Self {
        Self {
            boundary: format!("pmt-{}", ulid::Ulid::generate()),
            body: Vec::new(),
        }
    }

    /// Adds a text field.
    #[must_use]
    pub fn text(mut self, name: &str, value: &str) -> Self {
        self.body.extend_from_slice(
            format!(
                "--{}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n",
                self.boundary
            )
            .as_bytes(),
        );
        self
    }

    /// Adds a file field.
    #[must_use]
    pub fn file(mut self, name: &str, file_name: &str, mime: &str, bytes: &[u8]) -> Self {
        let safe = file_name.replace(['"', '\r', '\n'], "_");
        self.body.extend_from_slice(
            format!(
                "--{}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{safe}\"\r\nContent-Type: {mime}\r\n\r\n",
                self.boundary
            )
            .as_bytes(),
        );
        self.body.extend_from_slice(bytes);
        self.body.extend_from_slice(b"\r\n");
        self
    }

    /// The content type and the finished body.
    #[must_use]
    pub fn finish(mut self) -> (String, Vec<u8>) {
        self.body
            .extend_from_slice(format!("--{}--\r\n", self.boundary).as_bytes());
        (
            format!("multipart/form-data; boundary={}", self.boundary),
            self.body,
        )
    }
}

/// Base64 of `bytes`.
#[must_use]
pub fn b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// Bytes of base64 `text`.
///
/// # Errors
///
/// Invalid base64.
pub fn unb64(text: &str) -> Result<Vec<u8>, String> {
    STANDARD
        .decode(text.trim())
        .map_err(|e| format!("invalid base64 audio or image: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_urls_decode_and_errors_carry_the_providers_message() {
        let (bytes, mime) = data_url("data:image/png;base64,iVBORw==").unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(bytes, [0x89, b'P', b'N', b'G']);
        assert_eq!(
            status_error(401, br#"{"error": {"message": "bad key"}}"#),
            "HTTP 401: bad key"
        );
        assert_eq!(status_error(503, b"<html>"), "HTTP 503");
    }

    #[test]
    fn multipart_bodies_frame_each_field() {
        let (kind, body) = Multipart::new()
            .text("model", "whisper-1")
            .file("file", "a.ogg", "audio/ogg", b"OggS")
            .finish();
        let boundary = kind.strip_prefix("multipart/form-data; boundary=").unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.starts_with(&format!("--{boundary}\r\n")));
        assert!(text.contains("name=\"model\"\r\n\r\nwhisper-1\r\n"));
        assert!(text.contains("filename=\"a.ogg\"\r\nContent-Type: audio/ogg\r\n\r\nOggS\r\n"));
        assert!(text.ends_with(&format!("--{boundary}--\r\n")));
    }
}
