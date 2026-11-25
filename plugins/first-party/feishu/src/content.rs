//! What an inbound Feishu message says: its text (mentions resolved, the
//! bot's own mention removed), the files it carries, and whether it
//! mentions the bot. Content is a JSON string whose shape depends on
//! `message_type` (`text`, `post` rich text, `image`, `file`, `audio`,
//! `media`, `sticker`, ...).

use serde_json::Value;

/// A file a message carries, not yet downloaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRef {
    /// `image`, `voice`, `audio`, `video` or `document`.
    pub kind: &'static str,
    /// The image or file key.
    pub key: String,
    /// The resource type to download it as (`image` or `file`).
    pub resource: &'static str,
    /// Its name.
    pub name: String,
}

/// A message's meaning.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Content {
    /// The text.
    pub text: String,
    /// The files.
    pub files: Vec<FileRef>,
    /// It mentions the bot (or everyone).
    pub mentioned: bool,
}

fn image(key: &str) -> FileRef {
    FileRef {
        kind: "image",
        key: key.to_owned(),
        resource: "image",
        name: format!("{key}.png"),
    }
}

/// Renders one element of a rich-text (`post`) paragraph.
fn post_element(element: &Value, files: &mut Vec<FileRef>) -> String {
    let text = |key: &str| element[key].as_str().unwrap_or_default().to_owned();
    match element["tag"].as_str().unwrap_or_default() {
        "text" | "md" => text("text"),
        "a" => format!("[{}]({})", text("text"), text("href")),
        "at" => format!("@{}", text("user_name")),
        "code_block" => format!("```{}\n{}\n```", text("language"), text("text")),
        "img" => {
            files.push(image(element["image_key"].as_str().unwrap_or_default()));
            String::new()
        }
        "media" => {
            files.push(FileRef {
                kind: "video",
                key: text("file_key"),
                resource: "file",
                name: "video.mp4".to_owned(),
            });
            String::new()
        }
        "emotion" => format!(":{}:", text("emoji_type")),
        _ => String::new(),
    }
}

/// The text of a `post`, whatever locale wraps it.
fn post(content: &Value, files: &mut Vec<FileRef>) -> String {
    let body = if content["content"].is_array() {
        content
    } else {
        content
            .as_object()
            .and_then(|m| m.values().find(|v| v["content"].is_array()))
            .unwrap_or(&Value::Null)
    };
    let mut lines = Vec::new();
    if let Some(title) = body["title"].as_str().filter(|t| !t.is_empty()) {
        lines.push(title.to_owned());
    }
    for paragraph in body["content"].as_array().into_iter().flatten() {
        let line: String = paragraph
            .as_array()
            .into_iter()
            .flatten()
            .map(|e| post_element(e, files))
            .collect();
        lines.push(line);
    }
    lines.join("\n").trim().to_owned()
}

/// A file of a `file`, `audio` or `media` message; `kind` `None` types a
/// plain file by its name.
fn file(content: &Value, kind: Option<&'static str>, default_name: &str) -> FileRef {
    let name = content["file_name"]
        .as_str()
        .unwrap_or(default_name)
        .to_owned();
    FileRef {
        kind: kind.unwrap_or_else(|| adapter_kit::message::kind_of_name(&name)),
        key: content["file_key"].as_str().unwrap_or_default().to_owned(),
        resource: "file",
        name,
    }
}

/// Replaces mention keys (`@_user_1`) with `@name`, dropping the bot's own
/// mention; whether the bot (or everyone) was mentioned.
fn resolve_mentions(text: &str, mentions: &[Value], bot: &str) -> (String, bool) {
    let mut out = text.to_owned();
    let mut mentioned = text.contains("@_all");
    for mention in mentions {
        let key = mention["key"].as_str().unwrap_or_default();
        if key.is_empty() {
            continue;
        }
        let is_bot = !bot.is_empty() && mention["id"]["open_id"].as_str() == Some(bot);
        mentioned |= is_bot;
        let shown = if is_bot {
            String::new()
        } else {
            format!("@{}", mention["name"].as_str().unwrap_or_default())
        };
        out = out.replace(key, &shown);
    }
    // A removed mention leaves doubled spaces; line breaks stay.
    let tidy: Vec<String> = out
        .lines()
        .map(|l| {
            l.split(' ')
                .filter(|w| !w.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    (tidy.join("\n").trim().to_owned(), mentioned)
}

/// The meaning of a message of `kind` with JSON `raw` content and
/// `mentions`, for the bot whose open id is `bot`.
#[must_use]
pub fn parse(kind: &str, raw: &str, mentions: &[Value], bot: &str) -> Content {
    let content: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
    let mut files = Vec::new();
    let text = match kind {
        "text" => content["text"].as_str().unwrap_or_default().to_owned(),
        "post" => post(&content, &mut files),
        "image" => {
            files.push(image(content["image_key"].as_str().unwrap_or_default()));
            String::new()
        }
        "file" => {
            files.push(file(&content, None, "file"));
            String::new()
        }
        "audio" => {
            files.push(file(&content, Some("voice"), "voice.opus"));
            String::new()
        }
        "media" => {
            files.push(file(&content, Some("video"), "video.mp4"));
            String::new()
        }
        "sticker" => "[sticker]".to_owned(),
        "location" => format!(
            "[location: {}]",
            content["name"].as_str().unwrap_or_default()
        ),
        other => format!("[{other} message]"),
    };
    let (text, mentioned) = resolve_mentions(&text, mentions, bot);
    Content {
        text,
        files: files.into_iter().filter(|f| !f.key.is_empty()).collect(),
        mentioned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_bots_mention_is_dropped_and_others_named() {
        let mentions = [
            json!({"key": "@_user_1", "id": {"open_id": "ou_bot"}, "name": "Bot"}),
            json!({"key": "@_user_2", "id": {"open_id": "ou_kim"}, "name": "Kim"}),
        ];
        let c = parse(
            "text",
            r#"{"text":"@_user_1 ask @_user_2 please"}"#,
            &mentions,
            "ou_bot",
        );
        assert_eq!(c.text, "ask @Kim please");
        assert!(c.mentioned);
        assert!(!parse("text", r#"{"text":"hi"}"#, &[], "ou_bot").mentioned);
    }

    #[test]
    fn posts_render_text_and_collect_images() {
        let raw = json!({"zh_cn": {"title": "T", "content": [[{"tag": "text", "text": "see "}, {"tag": "a", "text": "doc", "href": "https://x"}], [{"tag": "img", "image_key": "img_1"}]]}}).to_string();
        let c = parse("post", &raw, &[], "");
        assert_eq!(c.text, "T\nsee [doc](https://x)");
        assert_eq!(c.files[0].key, "img_1");
        let f = parse("file", r#"{"file_key":"f1","file_name":"a.pdf"}"#, &[], "");
        assert_eq!(
            (f.files[0].kind, f.files[0].name.as_str()),
            ("document", "a.pdf")
        );
    }
}
