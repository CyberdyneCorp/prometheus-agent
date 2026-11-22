//! Document comments (`drive.notice.comment_add_v1`, delivered when the
//! app has drive comment permissions): a comment or reply addressed to the
//! bot becomes a turn in chat `comment:<file_type>:<file_token>:<comment
//! id>:<whole|local>` whose text carries the document's text and the
//! thread (the last 20 messages of whole-document comments, the last 12
//! of a comment on a selection, with the quoted selection). The commenter
//! must pass the comment rules ([`crate::rules`]); otherwise no turn
//! starts. The reply goes back into the thread in pieces of at most 4000
//! characters: a reply to a selection comment (a new whole-document
//! comment where Feishu refuses replies there), a new whole-document
//! comment otherwise. A reply of exactly `NO_REPLY` posts nothing.

use adapter_kit::message::Inbound;
use adapter_kit::{host, text};
use serde_json::{Value, json};

use crate::api::{Api, code_of};
use crate::rules;

const WHOLE_LIMIT: usize = 20;
const LOCAL_LIMIT: usize = 12;
/// The longest piece of a comment reply.
pub const CHUNK: usize = 4000;
const DOC_TEXT_LIMIT: usize = 8000;
const MAX_PAGES: usize = 5;
/// Feishu refuses a reply in this thread.
const REPLY_NOT_ALLOWED: i64 = 1_069_302;
const NO_REPLY: &str = "NO_REPLY";

/// The comment a chat id names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    /// `docx`, `doc`, `sheet`, ...
    pub file_type: String,
    /// The document's token.
    pub file_token: String,
    /// The comment.
    pub comment_id: String,
    /// A whole-document comment (else a comment on a selection).
    pub whole: bool,
}

impl Thread {
    /// Its chat id.
    #[must_use]
    pub fn chat(&self) -> String {
        format!(
            "comment:{}:{}:{}:{}",
            self.file_type,
            self.file_token,
            self.comment_id,
            if self.whole { "whole" } else { "local" }
        )
    }

    /// The thread of a comment chat id.
    #[must_use]
    pub fn of(chat: &str) -> Option<Self> {
        let mut parts = chat.strip_prefix("comment:")?.split(':');
        let (file_type, file_token, comment_id, kind) =
            (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        Some(Self {
            file_type: file_type.to_owned(),
            file_token: file_token.to_owned(),
            comment_id: comment_id.to_owned(),
            whole: kind == "whole",
        })
    }
}

/// The fields of a comment event that matter.
struct Notice {
    thread: Thread,
    reply_id: String,
    from: String,
    to: String,
    notice: String,
}

fn notice(event: &Value) -> Option<Notice> {
    let meta = &event["notice_meta"];
    let text = |v: &Value| v.as_str().unwrap_or_default().to_owned();
    let notice = Notice {
        thread: Thread {
            file_type: text(&meta["file_type"]),
            file_token: text(&meta["file_token"]),
            comment_id: text(&event["comment_id"]),
            whole: false,
        },
        reply_id: text(&event["reply_id"]),
        from: text(&meta["from_user_id"]["open_id"]),
        to: text(&meta["to_user_id"]["open_id"]),
        notice: text(&meta["notice_type"]),
    };
    let complete = !notice.thread.file_type.is_empty()
        && !notice.thread.file_token.is_empty()
        && !notice.thread.comment_id.is_empty()
        && !notice.from.is_empty();
    complete.then_some(notice)
}

/// The plain text of a comment reply's elements.
fn reply_text(reply: &Value) -> String {
    let content = match &reply["content"] {
        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
        other => other.clone(),
    };
    content["elements"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| match e["type"].as_str().unwrap_or_default() {
            "text_run" => e["text_run"]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            "docs_link" => e["docs_link"]["url"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            "person" => format!("@{}", e["person"]["user_id"].as_str().unwrap_or_default()),
            _ => String::new(),
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

fn user_of(reply: &Value) -> String {
    match &reply["user_id"] {
        Value::String(s) => s.clone(),
        other => other["open_id"].as_str().unwrap_or_default().to_owned(),
    }
}

/// `items` of every page of a listing (at most five).
fn pages(api: &mut Api, path: &str) -> Vec<Value> {
    let mut items = Vec::new();
    let mut token = String::new();
    for _ in 0..MAX_PAGES {
        let url = if token.is_empty() {
            path.to_owned()
        } else {
            format!("{path}&page_token={}", text::percent_encode(&token))
        };
        let Ok(data) = api.call("GET", &url, None) else {
            break;
        };
        items.extend(data["items"].as_array().cloned().unwrap_or_default());
        token = data["page_token"].as_str().unwrap_or_default().to_owned();
        if data["has_more"] != true || token.is_empty() {
            break;
        }
    }
    items
}

/// The thread's lines, oldest first, the last `limit` of them.
fn timeline(replies: &[Value], bot: &str, limit: usize) -> Vec<String> {
    let lines: Vec<String> = replies
        .iter()
        .map(|r| {
            let user = user_of(r);
            let me = if !bot.is_empty() && user == bot {
                " (you)"
            } else {
                ""
            };
            format!("[{user}{me}] {}", reply_text(r))
        })
        .collect();
    let start = lines.len().saturating_sub(limit);
    lines[start..].to_vec()
}

/// The document's text (docx only), cut at 8000 characters.
fn document_text(api: &mut Api, thread: &Thread) -> String {
    if thread.file_type != "docx" {
        return format!(
            "(the text of a {} document is not fetched; read it with feishu_doc_read when it is a docx)",
            thread.file_type
        );
    }
    let path = format!(
        "/open-apis/docx/v1/documents/{}/raw_content",
        thread.file_token
    );
    match api.call("GET", &path, None) {
        Ok(data) => {
            let text = data["content"].as_str().unwrap_or_default();
            let mut cut: String = text.chars().take(DOC_TEXT_LIMIT).collect();
            if cut.len() < text.len() {
                cut.push_str("\n[... truncated; feishu_doc_read has the rest]");
            }
            cut
        }
        Err(why) => format!("(the document text could not be read: {why})"),
    }
}

/// The document's title and link.
fn meta(api: &mut Api, thread: &Thread) -> (String, String) {
    let body = json!({"request_docs": [{"doc_token": thread.file_token, "doc_type": thread.file_type}], "with_url": true});
    let data = api
        .call("POST", "/open-apis/drive/v1/metas/batch_query", Some(&body))
        .unwrap_or_default();
    let meta = &data["metas"][0];
    (
        meta["title"].as_str().unwrap_or("Untitled").to_owned(),
        meta["url"].as_str().unwrap_or_default().to_owned(),
    )
}

/// The comment's details: whether it is whole-document, its quote.
fn comment(api: &mut Api, thread: &Thread) -> Value {
    let path = format!(
        "/open-apis/drive/v1/files/{}/comments/batch_query?file_type={}&user_id_type=open_id",
        thread.file_token, thread.file_type
    );
    let body = json!({"comment_ids": [thread.comment_id]});
    api.call("POST", &path, Some(&body))
        .map(|d| d["items"][0].clone())
        .unwrap_or_default()
}

/// The thread's replies: every whole-document comment's, or this one's.
fn replies(api: &mut Api, thread: &Thread) -> Vec<Value> {
    if thread.whole {
        let path = format!(
            "/open-apis/drive/v1/files/{}/comments?file_type={}&is_whole=true&page_size=100&user_id_type=open_id",
            thread.file_token, thread.file_type
        );
        pages(api, &path)
            .iter()
            .flat_map(|c| {
                c["reply_list"]["replies"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .collect()
    } else {
        let path = format!(
            "/open-apis/drive/v1/files/{}/comments/{}/replies?file_type={}&page_size=100&user_id_type=open_id",
            thread.file_token, thread.comment_id, thread.file_type
        );
        pages(api, &path)
    }
}

fn prompt(
    thread: &Thread,
    title: &str,
    url: &str,
    quote: &str,
    doc: &str,
    lines: &[String],
) -> String {
    let (kind, limit) = if thread.whole {
        ("a whole-document comment", WHOLE_LIMIT)
    } else {
        ("a comment on a selection", LOCAL_LIMIT)
    };
    let mut out = format!(
        "You were mentioned in {kind} on the Feishu document \"{title}\" ({url}; file_type={}, file_token={}, comment_id={}).\n",
        thread.file_type, thread.file_token, thread.comment_id
    );
    if !quote.is_empty() {
        out.push_str(&format!("Quoted selection: \"{quote}\"\n"));
    }
    out.push_str(&format!(
        "\nComment thread (last {} of at most {limit}):\n{}\n",
        lines.len(),
        lines.join("\n")
    ));
    out.push_str(&format!("\nDocument text:\n{doc}\n"));
    out.push_str(
        "\nYour reply is posted into this comment thread as plain text. \
         Answer the latest comment; reply with exactly NO_REPLY when no answer is needed.",
    );
    out
}

/// The turn a comment event starts, if any: not the bot's own comment,
/// addressed to the bot, a new comment or reply, and from a commenter the
/// rules allow.
pub fn turn(api: &mut Api, event: &Value, bot: &str) -> Option<Inbound> {
    let mut notice = notice(event)?;
    let addressed = bot.is_empty() || notice.to == bot;
    let new =
        notice.notice.is_empty() || notice.notice == "add_comment" || notice.notice == "add_reply";
    if notice.from == bot || !addressed || !new {
        return None;
    }
    let rule = rules::resolve(
        &rules::load(),
        &notice.thread.file_type,
        &notice.thread.file_token,
    );
    if !rules::allows(&rule, &notice.from, &rules::approved()) {
        host::info(&format!(
            "Feishu comment by {} on {}:{} not answered ({} rule, policy {}); approve with \
             `prometheus gateway feishu comment-rules pairing add {}`",
            notice.from,
            notice.thread.file_type,
            notice.thread.file_token,
            rule.source,
            rule.policy,
            notice.from
        ));
        return None;
    }
    let detail = comment(api, &notice.thread);
    notice.thread.whole = detail["is_whole"].as_bool().unwrap_or(false);
    let (title, url) = meta(api, &notice.thread);
    let limit = if notice.thread.whole {
        WHOLE_LIMIT
    } else {
        LOCAL_LIMIT
    };
    let lines = timeline(&replies(api, &notice.thread), bot, limit);
    let doc = document_text(api, &notice.thread);
    let quote = detail["quote"].as_str().unwrap_or_default();
    let id = if notice.reply_id.is_empty() {
        notice.thread.comment_id.clone()
    } else {
        notice.reply_id.clone()
    };
    let mut message = Inbound::text(
        &format!("comment-{id}"),
        &notice.thread.chat(),
        "thread",
        &notice.from,
        &prompt(&notice.thread, &title, &url, quote, &doc, &lines),
    );
    message.mentioned = true;
    Some(message)
}

/// Feishu's comment text cannot hold raw `<`, `>` or `&`.
fn escaped(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn add_whole(api: &mut Api, thread: &Thread, piece: &str) -> Result<Value, String> {
    let body = json!({"file_type": thread.file_type, "reply_elements": [{"type": "text", "text": escaped(piece)}]});
    let path = format!(
        "/open-apis/drive/v1/files/{}/new_comments",
        thread.file_token
    );
    api.call("POST", &path, Some(&body))
}

fn reply(api: &mut Api, thread: &Thread, piece: &str) -> Result<Value, String> {
    let body = json!({"content": {"elements": [{"type": "text_run", "text_run": {"text": escaped(piece)}}]}});
    let path = format!(
        "/open-apis/drive/v1/files/{}/comments/{}/replies?file_type={}",
        thread.file_token, thread.comment_id, thread.file_type
    );
    api.call("POST", &path, Some(&body))
}

/// Posts `text` into `thread`: the id of the last piece posted.
///
/// # Errors
///
/// Feishu refused a piece (classified).
pub fn deliver(api: &mut Api, thread: &Thread, text: &str) -> Result<String, String> {
    if text.trim() == NO_REPLY {
        return Ok(String::new());
    }
    let mut whole = thread.whole;
    let mut last = String::new();
    for piece in text::split_chars(text.trim(), CHUNK) {
        let answer = if whole {
            add_whole(api, thread, &piece)?
        } else {
            match reply(api, thread, &piece) {
                Err(why) if code_of(&why) == Some(REPLY_NOT_ALLOWED) => {
                    whole = true;
                    add_whole(api, thread, &piece)?
                }
                other => other?,
            }
        };
        last = answer["reply_id"]
            .as_str()
            .or(answer["comment_id"].as_str())
            .unwrap_or_default()
            .to_owned();
    }
    Ok(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_chats_round_trip_and_threads_keep_their_last_lines() {
        let thread = Thread {
            file_type: "docx".into(),
            file_token: "doc1".into(),
            comment_id: "c9".into(),
            whole: true,
        };
        assert_eq!(Thread::of(&thread.chat()), Some(thread));
        assert_eq!(Thread::of("oc_123"), None);
        let replies: Vec<Value> = (0..25)
            .map(|i| json!({"user_id": if i == 24 { "ou_bot" } else { "ou_a" }, "content": {"elements": [{"type": "text_run", "text_run": {"text": format!("m{i}")}}]}}))
            .collect();
        let lines = timeline(&replies, "ou_bot", WHOLE_LIMIT);
        assert_eq!(lines.len(), 20);
        assert_eq!(lines[0], "[ou_a] m5");
        assert_eq!(lines[19], "[ou_bot (you)] m24");
        assert_eq!(escaped("a<b>&"), "a&lt;b&gt;&amp;");
    }
}
