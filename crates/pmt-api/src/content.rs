//! OpenAI message content (spec: api-server, Chat Completions endpoint;
//! Responses endpoint): a string, or `text` / `input_text` parts and
//! `image_url` / `input_image` parts whose URL is remote `http(s)` or a
//! `data:image/*` URL. Uploaded files (`file`, `input_file`, `file_id`), a
//! non-image data URL or any other part are refused before a turn starts.

use base64::Engine as _;
use serde_json::Value;

use crate::host::ApiImage;
use crate::http::{Reply, bad_request};

/// A user message's text and images.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UserInput {
    pub text: String,
    pub images: Vec<ApiImage>,
}

/// The text of `content` (parts joined by blank lines), ignoring images.
pub(crate) fn text_of(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str().or_else(|| p.as_str()))
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

/// Parses a user message's `content`.
pub(crate) fn user_input(content: &Value) -> Result<UserInput, Reply> {
    let parts = match content {
        Value::String(text) => {
            return Ok(UserInput {
                text: text.clone(),
                images: Vec::new(),
            });
        }
        Value::Array(parts) => parts,
        Value::Null => return Ok(UserInput::default()),
        _ => {
            return Err(bad_request(
                "invalid_content",
                "content must be a string or a list of parts",
            ));
        }
    };
    let mut input = UserInput::default();
    let mut texts = Vec::new();
    for part in parts {
        match part["type"].as_str().unwrap_or_default() {
            "text" | "input_text" | "output_text" => {
                texts.push(part["text"].as_str().unwrap_or_default().to_owned());
            }
            "image_url" | "input_image" => input.images.push(image(part)?),
            other => return Err(unsupported(other)),
        }
    }
    input.text = texts.join("\n\n");
    Ok(input)
}

fn image(part: &Value) -> Result<ApiImage, Reply> {
    if part.get("file_id").is_some() {
        return Err(unsupported("file_id"));
    }
    let url = part["image_url"]["url"]
        .as_str()
        .or_else(|| part["image_url"].as_str())
        .or_else(|| part["url"].as_str())
        .unwrap_or_default();
    if url.starts_with("http://") || url.starts_with("https://") {
        return Ok(ApiImage::Url(url.to_owned()));
    }
    data_url(url).ok_or_else(|| unsupported("a non-image data URL"))
}

/// `data:image/<x>;base64,<bytes>`.
fn data_url(url: &str) -> Option<ApiImage> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let media_type = meta.strip_suffix(";base64")?;
    if !media_type.starts_with("image/") {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .ok()?;
    Some(ApiImage::Data {
        media_type: media_type.to_owned(),
        bytes,
    })
}

fn unsupported(what: &str) -> Reply {
    bad_request(
        "unsupported_content_type",
        &format!(
            "content of type {what} is not supported; send text and images (http(s) or data:image/* URLs)"
        ),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{ApiImage, user_input};

    /// Scenario: Uploaded file refused.
    #[test]
    fn files_and_non_image_data_urls_are_refused() {
        for part in [
            json!({"type": "file", "file": {"file_id": "f1"}}),
            json!({"type": "input_file", "file_data": "x"}),
            json!({"type": "image_url", "image_url": {"url": "data:application/pdf;base64,AAAA"}}),
            json!({"type": "input_image", "file_id": "f1"}),
        ] {
            let refused = user_input(&json!([part])).unwrap_err();
            assert_eq!(refused.status(), 400, "{part}");
        }
    }

    #[test]
    fn text_and_image_parts_are_read() {
        let input = user_input(&json!([
            {"type": "text", "text": "what is this?"},
            {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}},
            {"type": "input_image", "image_url": "data:image/png;base64,iVBORw=="},
        ]))
        .unwrap();
        assert_eq!(input.text, "what is this?");
        assert_eq!(
            input.images[0],
            ApiImage::Url("https://example.com/a.png".into())
        );
        assert!(
            matches!(&input.images[1], ApiImage::Data { media_type, .. } if media_type == "image/png")
        );
    }
}
