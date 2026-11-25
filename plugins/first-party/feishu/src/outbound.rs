//! Sending: text as a plain `text` message, Markdown as a rich `post`
//! (each fenced code block its own row, so Feishu's renderer cannot
//! swallow what follows) falling back to plain text when Feishu refuses
//! the post's content; buttons as an interactive card whose presses come
//! back as card callbacks; files uploaded first (images to the image
//! store, anything else to the file store). Replies use the reply API
//! (in the topic for threaded chats) and fall back to a new message when
//! the replied-to message is gone. Edits, deletes and reactions act on
//! sent messages.

use adapter_kit::message::{OutFile, Outgoing};
use adapter_kit::{net, text};
use serde_json::{Value, json};

use crate::Feishu;
use crate::api::code_of;
use crate::comments::{self, Thread};

/// Codes refusing a message's content (a `post` Feishu cannot render).
const BAD_CONTENT: &[i64] = &[230_001, 230_099];
/// Codes meaning the replied-to message is gone.
const REPLY_GONE: &[i64] = &[230_011, 231_003];

/// Whether `text` looks like Markdown worth a rich post.
fn is_markdown(text: &str) -> bool {
    text.contains("```")
        || text.contains("**")
        || text.contains("](")
        || text.contains('`')
        || text.lines().any(|l| {
            let l = l.trim_start();
            l.starts_with('#')
                || l.starts_with("- ")
                || l.starts_with("* ")
                || l.starts_with("> ")
                || l.starts_with('|')
                || l.split_once(". ")
                    .is_some_and(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
}

/// The rows of a Markdown post: prose and each fenced block apart.
fn post_rows(text: &str) -> Vec<Value> {
    let mut rows = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut fenced = false;
    let flush = |lines: &mut Vec<&str>, rows: &mut Vec<Value>| {
        let segment = lines.join("\n");
        if !segment.trim().is_empty() {
            rows.push(json!([{"tag": "md", "text": segment}]));
        }
        lines.clear();
    };
    for line in text.lines() {
        let fence = line.trim_start().starts_with("```");
        if fence && !fenced {
            flush(&mut current, &mut rows);
        }
        current.push(line);
        if fence {
            fenced = !fenced;
            if !fenced {
                flush(&mut current, &mut rows);
            }
        }
    }
    flush(&mut current, &mut rows);
    if rows.is_empty() {
        rows.push(json!([{"tag": "md", "text": text}]));
    }
    rows
}

/// A message type and its content for `text`.
fn content(text: &str, plain: bool) -> (&'static str, String) {
    if !plain && is_markdown(text) {
        let post = json!({"zh_cn": {"content": post_rows(text)}});
        ("post", post.to_string())
    } else {
        let shown = if plain {
            text::strip_markdown(text)
        } else {
            text.to_owned()
        };
        ("text", json!({"text": shown}).to_string())
    }
}

/// An interactive card showing `text` with `buttons` (label, data).
fn card(text: &str, buttons: &[(String, String)]) -> Value {
    let mut elements = vec![json!({"tag": "markdown", "content": text})];
    if !buttons.is_empty() {
        let actions: Vec<Value> = buttons
            .iter()
            .map(|(label, data)| {
                json!({"tag": "button", "type": "default",
                    "text": {"tag": "plain_text", "content": label},
                    "value": {"prometheus": data}})
            })
            .collect();
        elements.push(json!({"tag": "action", "actions": actions}));
    }
    json!({"config": {"wide_screen_mode": true, "update_multi": true}, "elements": elements})
}

/// The receive id type of a chat id.
fn receiver(chat: &str) -> &'static str {
    if chat.starts_with("ou_") {
        "open_id"
    } else if chat.starts_with("on_") {
        "union_id"
    } else {
        "chat_id"
    }
}

/// Feishu's name for a reaction emoji.
fn emoji_type(emoji: &str) -> &'static str {
    match emoji.trim_end_matches('\u{fe0f}') {
        "\u{1f440}" | "eyes" => "GLANCE",
        "\u{2705}" | "white_check_mark" | "\u{2714}" => "DONE",
        "\u{274c}" | "x" => "CrossMark",
        "\u{1f44d}" | "+1" | "thumbsup" => "THUMBSUP",
        "\u{1f44e}" | "-1" | "thumbsdown" => "ThumbsDown",
        "\u{2764}" | "heart" => "HEART",
        "\u{1f525}" | "fire" => "Fire",
        "\u{1f389}" | "tada" => "PARTY",
        "\u{1f914}" | "thinking" => "THINKING",
        _ => "OK",
    }
}

/// The upload of a file that is not an image: its file type and the
/// message type that shows it.
fn file_kind(file: &OutFile) -> (&'static str, &'static str) {
    let ext = file
        .name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "opus" | "ogg" => ("opus", "audio"),
        "mp4" => ("mp4", "media"),
        "pdf" => ("pdf", "file"),
        "doc" | "docx" => ("doc", "file"),
        "xls" | "xlsx" => ("xls", "file"),
        "ppt" | "pptx" => ("ppt", "file"),
        _ => ("stream", "file"),
    }
}

fn id_of(data: &Value) -> String {
    data["message_id"].as_str().unwrap_or_default().to_owned()
}

impl Feishu {
    /// Sends `msg_type` `content` to the chat of `out`, as a reply when
    /// it answers a message.
    fn create(&mut self, out: &Outgoing, msg_type: &str, content: &str) -> Result<String, String> {
        if let Some(target) = &out.reply_to {
            let body = json!({"msg_type": msg_type, "content": content, "reply_in_thread": out.thread.is_some()});
            let path = format!("/open-apis/im/v1/messages/{target}/reply");
            match self.api.call("POST", &path, Some(&body)) {
                Err(why) if code_of(&why).is_some_and(|c| REPLY_GONE.contains(&c)) => {}
                other => return other.map(|d| id_of(&d)),
            }
        }
        let body = json!({"receive_id": out.chat, "msg_type": msg_type, "content": content});
        let path = format!(
            "/open-apis/im/v1/messages?receive_id_type={}",
            receiver(&out.chat)
        );
        self.api.call("POST", &path, Some(&body)).map(|d| id_of(&d))
    }

    /// Sends text: a post when it is Markdown, plain text when Feishu
    /// refuses the post's content.
    fn text(&mut self, out: &Outgoing) -> Result<String, String> {
        let (kind, body) = content(&out.text, false);
        match self.create(out, kind, &body) {
            Err(why)
                if kind == "post" && code_of(&why).is_some_and(|c| BAD_CONTENT.contains(&c)) =>
            {
                let (kind, body) = content(&out.text, true);
                self.create(out, kind, &body)
            }
            other => other,
        }
    }

    fn upload(&mut self, file: &OutFile) -> Result<(String, &'static str, String), String> {
        let image = file.kind == "image" && !file.as_document;
        let (path, field, fields, message) = if image {
            (
                "/open-apis/im/v1/images",
                "image",
                vec![("image_type", "message")],
                "image",
            )
        } else {
            let (file_type, message) = file_kind(file);
            (
                "/open-apis/im/v1/files",
                "file",
                vec![("file_type", file_type), ("file_name", file.name.as_str())],
                message,
            )
        };
        let (content_type, body) =
            net::multipart(&fields, (field, &file.name, &file.mime, &file.data));
        let response = self.api.raw("POST", path, &content_type, Some(body))?;
        let answer = response.json();
        if let Some(why) = crate::api::refusal(response.status, &answer) {
            return Err(why);
        }
        let key_name = if image { "image_key" } else { "file_key" };
        let key = answer["data"][key_name]
            .as_str()
            .ok_or_else(|| format!("server: Feishu returned no {key_name}"))?
            .to_owned();
        Ok((key_name.to_owned(), message, key))
    }

    /// Sends `out`: files, then the text (as a card when it has buttons).
    pub(crate) fn send_message(&mut self, out: &Outgoing) -> Result<String, String> {
        if let Some(thread) = Thread::of(&out.chat) {
            return comments::deliver(&mut self.api, &thread, &out.text);
        }
        let mut last = String::new();
        for file in &out.attachments {
            let (key_name, message, key) = self.upload(file)?;
            last = self.create(out, message, &json!({key_name: key}).to_string())?;
        }
        if !out.buttons.is_empty() {
            let buttons: Vec<(String, String)> = out
                .buttons
                .iter()
                .map(|b| (b.label.clone(), b.data.clone()))
                .collect();
            let body = card(&out.text, &buttons).to_string();
            last = self.create(out, "interactive", &body)?;
            self.remember_card(&last);
        } else if !out.text.trim().is_empty() {
            last = self.text(out)?;
        }
        if last.is_empty() && out.attachments.is_empty() {
            return Err("permanent: nothing to send".to_owned());
        }
        Ok(last)
    }

    fn remember_card(&mut self, id: &str) {
        self.cards_sent.push(id.to_owned());
        if self.cards_sent.len() > 512 {
            self.cards_sent.remove(0);
        }
    }

    /// Edits a sent message: a card keeps being a card (its buttons gone),
    /// text and posts are replaced.
    pub(crate) fn edit(&mut self, out: &Outgoing) -> Result<String, String> {
        let id = out
            .message_id
            .clone()
            .ok_or("permanent: no message to edit")?;
        let path = format!("/open-apis/im/v1/messages/{id}");
        if self.cards_sent.contains(&id) {
            let body = json!({"content": card(&out.text, &[]).to_string()});
            self.api.call("PATCH", &path, Some(&body))?;
        } else {
            let (kind, content) = content(&out.text, false);
            let body = json!({"msg_type": kind, "content": content});
            self.api.call("PUT", &path, Some(&body))?;
        }
        Ok(id)
    }

    /// Deletes a sent message.
    pub(crate) fn delete(&mut self, out: &Outgoing) -> Result<String, String> {
        let id = out
            .message_id
            .clone()
            .ok_or("permanent: no message to delete")?;
        self.api
            .call("DELETE", &format!("/open-apis/im/v1/messages/{id}"), None)?;
        Ok(id)
    }

    /// Adds or removes a reaction.
    pub(crate) fn react(&mut self, out: &Outgoing) -> Result<String, String> {
        let id = out
            .message_id
            .clone()
            .ok_or("permanent: no message to react to")?;
        let kind = emoji_type(out.emoji.as_deref().unwrap_or_default());
        let path = format!("/open-apis/im/v1/messages/{id}/reactions");
        if out.remove {
            let Some(at) = self
                .reactions
                .iter()
                .position(|(m, k, _)| *m == id && *k == kind)
            else {
                return Ok(id);
            };
            let (_, _, reaction) = self.reactions.remove(at);
            self.api
                .call("DELETE", &format!("{path}/{reaction}"), None)?;
            return Ok(id);
        }
        let body = json!({"reaction_type": {"emoji_type": kind}});
        let data = self.api.call("POST", &path, Some(&body))?;
        let reaction = data["reaction_id"].as_str().unwrap_or_default().to_owned();
        self.reactions.push((id.clone(), kind, reaction));
        if self.reactions.len() > 512 {
            self.reactions.remove(0);
        }
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_becomes_a_post_with_code_blocks_on_their_own_rows() {
        assert!(!is_markdown("hello there"));
        assert!(is_markdown("**bold**") && is_markdown("1. one") && is_markdown("# Title"));
        let rows = post_rows("intro\n```rust\nfn x() {}\n```\nafter");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1][0]["text"], "```rust\nfn x() {}\n```");
        let (kind, body) = content("**hi**", true);
        assert_eq!(kind, "text");
        assert!(body.contains("\"hi\""), "{body}");
        assert_eq!(receiver("ou_1"), "open_id");
        assert_eq!(receiver("oc_1"), "chat_id");
        assert_eq!(emoji_type("\u{1f440}"), "GLANCE");
    }
}
