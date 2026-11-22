//! Sends through the Graph API `/<phone number id>/messages`.
//!
//! Choices become interactive messages: an approval shows Approve and
//! Deny reply buttons (its body numbers every choice, and a numbered
//! answer picks one), other choices show as up to three reply buttons
//! (labels cut to 20 characters) or a list of up to ten rows (titles cut
//! to 24), more as numbered text. Files are uploaded to `/media` first; a
//! voice note goes up as OGG/Opus. Graph errors are classified by their
//! code: throttling (`130429`, `131056`, `80007`, `4`) is rate limited,
//! `131047` is the closed 24-hour window.

use adapter_kit::message::{Button, OutFile, Outgoing};
use adapter_kit::{host, net, text};
use serde_json::{Value, json};

use crate::{Choices, Settings, auth, with};

const BUTTON_CHARS: usize = 20;
const ROW_CHARS: usize = 24;
const BODY_CHARS: usize = 1024;
const MAX_BUTTONS: usize = 3;
const MAX_ROWS: usize = 10;
/// Graph's error for a message outside the customer-service window.
const WINDOW_CLOSED: i64 = 131_047;
/// What a failed send says when no re-engagement template is configured.
pub(crate) const WINDOW_CLOSED_TEXT: &str = "WhatsApp 24-hour window closed";

fn cut(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn messages_url(settings: &Settings) -> String {
    format!("{}/{}/messages", settings.graph, settings.phone_number_id)
}

/// Why a post failed: the closed window apart from the rest.
enum Refused {
    WindowClosed,
    Other(String),
}

/// Posts one message payload; its message id.
fn post_once(settings: &Settings, payload: &Value) -> Result<String, Refused> {
    let auth = auth().map_err(Refused::Other)?;
    let body = payload.to_string().into_bytes();
    let response = net::send(
        "POST",
        &messages_url(settings),
        &[
            ("authorization", auth.as_str()),
            ("content-type", "application/json"),
        ],
        Some(body),
    )
    .map_err(Refused::Other)?;
    if response.status < 300 {
        return Ok(response.json()["messages"][0]["id"]
            .as_str()
            .unwrap_or_default()
            .to_owned());
    }
    let error = &response.json()["error"];
    let code = error["code"].as_i64().unwrap_or_default();
    let message = error["message"].as_str().unwrap_or_default();
    Err(match code {
        WINDOW_CLOSED => Refused::WindowClosed,
        130_429 | 131_056 | 80_007 | 4 => Refused::Other("rate_limited".to_owned()),
        _ => Refused::Other(match net::classify(&response) {
            Err(why) if why.starts_with("permanent") => {
                format!("permanent: graph error {code}: {message}")
            }
            Err(why) => why,
            Ok(()) => format!("permanent: graph error {code}: {message}"),
        }),
    })
}

/// Posts `payload` to `chat`; outside the window, the re-engagement
/// template goes instead and `payload` waits for the user's answer.
fn post(settings: &Settings, chat: &str, payload: &Value) -> Result<String, String> {
    match post_once(settings, payload) {
        Ok(id) => Ok(id),
        Err(Refused::Other(why)) => Err(why),
        Err(Refused::WindowClosed) => {
            let Some(template) = &settings.template else {
                return Err(format!("permanent: {WINDOW_CLOSED_TEXT}"));
            };
            let invite = json!({
                "messaging_product": "whatsapp",
                "to": chat,
                "type": "template",
                "template": {"name": template, "language": {"code": settings.template_language}},
            });
            let id = post_once(settings, &invite).map_err(|refused| match refused {
                Refused::WindowClosed => format!("permanent: {WINDOW_CLOSED_TEXT}"),
                Refused::Other(why) => why,
            })?;
            hold(chat, payload);
            Ok(id)
        }
    }
}

fn held_key(chat: &str) -> String {
    format!("held:{chat}")
}

/// Keeps `payload` for when `chat` answers the template.
fn hold(chat: &str, payload: &Value) {
    let key = held_key(chat);
    let mut held = host::load(&key).unwrap_or_else(|| json!([]));
    if let Some(list) = held.as_array_mut() {
        list.push(payload.clone());
    }
    host::store(&key, &held);
}

/// Sends what waited for `chat` to answer (its window is open again).
pub(crate) fn held_after_reply(settings: &Settings, chat: &str) {
    let key = held_key(chat);
    let Some(held) = host::load(&key).filter(|h| h.as_array().is_some_and(|l| !l.is_empty()))
    else {
        return;
    };
    host::store(&key, &json!([]));
    for payload in held.as_array().into_iter().flatten() {
        if let Err(why) = post(settings, chat, payload) {
            host::warn(&format!("a held WhatsApp reply was not delivered: {why}"));
        }
    }
}

fn envelope(chat: &str, kind: &str, block: Value, reply_to: Option<&str>) -> Value {
    let mut payload = json!({
        "messaging_product": "whatsapp",
        "recipient_type": "individual",
        "to": chat,
        "type": kind,
    });
    payload[kind] = block;
    if let Some(id) = reply_to {
        payload["context"] = json!({"message_id": id});
    }
    payload
}

/// Sends one outgoing message: its files, then its text or choices.
pub(crate) fn send(settings: &Settings, out: &Outgoing) -> Result<String, String> {
    let mut last = String::new();
    for file in &out.attachments {
        last = send_file(settings, &out.chat, file)?;
    }
    let plain = text::strip_markdown(&out.text);
    if !out.buttons.is_empty() {
        let payload = interactive(&out.chat, &plain, &out.buttons);
        let id = post(settings, &out.chat, &payload)?;
        with(|s| {
            s.choices.push(Choices {
                chat: out.chat.clone(),
                data: out.buttons.iter().map(|b| b.data.clone()).collect(),
            });
            Ok(())
        })?;
        return Ok(id);
    }
    if !plain.trim().is_empty() {
        let block = json!({"body": plain, "preview_url": false});
        last = post(
            settings,
            &out.chat,
            &envelope(&out.chat, "text", block, out.reply_to.as_deref()),
        )?;
    }
    Ok(last)
}

fn numbered(text: &str, buttons: &[Button]) -> String {
    let lines: Vec<String> = buttons
        .iter()
        .enumerate()
        .map(|(i, b)| format!("{}. {}", i + 1, b.label))
        .collect();
    format!("{text}\n\n{}", lines.join("\n"))
}

fn reply_button(label: &str, data: &str) -> Value {
    json!({"type": "reply", "reply": {"id": data, "title": cut(label, BUTTON_CHARS)}})
}

/// The interactive message of a choice.
fn interactive(chat: &str, body: &str, buttons: &[Button]) -> Value {
    let body = if body.trim().is_empty() {
        "Choose:"
    } else {
        body
    };
    if buttons.iter().any(|b| b.data.starts_with("appr:")) {
        let pick = |choice: &str| {
            buttons
                .iter()
                .find(|b| b.data.contains(&format!(":{choice}:")))
        };
        let actions: Vec<Value> = [("Approve", pick("once")), ("Deny", pick("deny"))]
            .into_iter()
            .filter_map(|(label, b)| b.map(|b| reply_button(label, &b.data)))
            .collect();
        let block = json!({
            "type": "button",
            "body": {"text": cut(&numbered(body, buttons), BODY_CHARS)},
            "action": {"buttons": actions},
        });
        return envelope(chat, "interactive", block, None);
    }
    let block = match buttons.len() {
        0..=MAX_BUTTONS => json!({
            "type": "button",
            "body": {"text": cut(body, BODY_CHARS)},
            "action": {"buttons": buttons.iter().map(|b| reply_button(&b.label, &b.data)).collect::<Vec<_>>()},
        }),
        n if n <= MAX_ROWS => json!({
            "type": "list",
            "body": {"text": cut(body, BODY_CHARS)},
            "action": {
                "button": "Choose",
                "sections": [{
                    "title": "Choices",
                    "rows": buttons.iter().map(|b| json!({"id": b.data, "title": cut(&b.label, ROW_CHARS)})).collect::<Vec<_>>(),
                }],
            },
        }),
        _ => {
            let text = json!({"body": numbered(body, buttons), "preview_url": false});
            return envelope(chat, "text", text, None);
        }
    };
    envelope(chat, "interactive", block, None)
}

/// Uploads `file` and sends it; its message id.
fn send_file(settings: &Settings, chat: &str, file: &OutFile) -> Result<String, String> {
    let (kind, mime) = match file.kind.as_str() {
        _ if file.as_voice => ("audio", "audio/ogg; codecs=opus".to_owned()),
        "image" if !file.as_document => ("image", file.mime.clone()),
        "video" => ("video", file.mime.clone()),
        "audio" | "voice" => ("audio", file.mime.clone()),
        _ => ("document", file.mime.clone()),
    };
    let (content_type, body) = net::multipart(
        &[("messaging_product", "whatsapp"), ("type", &mime)],
        ("file", &file.name, &mime, &file.data),
    );
    let auth = auth()?;
    let response = net::send(
        "POST",
        &format!("{}/{}/media", settings.graph, settings.phone_number_id),
        &[("authorization", &auth), ("content-type", &content_type)],
        Some(body),
    )?;
    net::classify(&response)?;
    let media_id = response.json()["id"]
        .as_str()
        .ok_or("server: the upload returned no media id")?
        .to_owned();
    let mut block = json!({"id": media_id});
    if kind == "document" {
        block["filename"] = json!(file.name);
    }
    post(settings, chat, &envelope(chat, kind, block, None))
}

/// The typing indicator, on the chat's latest message.
pub(crate) fn typing(settings: &Settings, chat: &str) -> Result<(), String> {
    let last = with(|s| Ok(s.last.get(chat).cloned()))?;
    match last {
        Some(id) => crate::inbound::read_with_typing(settings, &id),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buttons(n: usize, prefix: &str) -> Vec<Button> {
        (1..=n)
            .map(|i| Button {
                label: format!("A rather long choice label {i}"),
                data: format!("{prefix}{i}"),
            })
            .collect()
    }

    #[test]
    fn choices_become_buttons_then_lists_then_text() {
        let three = interactive("1", "Pick", &buttons(3, "clar:q:1."));
        assert_eq!(three["interactive"]["type"], "button");
        let title = three["interactive"]["action"]["buttons"][0]["reply"]["title"]
            .as_str()
            .unwrap();
        assert_eq!(title.chars().count(), BUTTON_CHARS);
        let ten = interactive("1", "Pick", &buttons(10, "clar:q:1."));
        assert_eq!(ten["interactive"]["type"], "list");
        let row = ten["interactive"]["action"]["sections"][0]["rows"][9]["title"]
            .as_str()
            .unwrap();
        assert_eq!(row.chars().count(), ROW_CHARS);
        let eleven = interactive("1", "Pick", &buttons(11, "clar:q:1."));
        assert_eq!(eleven["type"], "text");
    }

    #[test]
    fn approvals_offer_approve_and_deny() {
        let choices: Vec<Button> = ["once", "session", "always", "deny"]
            .iter()
            .map(|c| Button {
                label: (*c).to_owned(),
                data: format!("appr:r1:{c}:mac"),
            })
            .collect();
        let card = interactive("1", "Approval needed: terminal", &choices);
        let actions = card["interactive"]["action"]["buttons"].as_array().unwrap();
        let titles: Vec<&str> = actions
            .iter()
            .map(|a| a["reply"]["title"].as_str().unwrap())
            .collect();
        assert_eq!(titles, ["Approve", "Deny"]);
        assert_eq!(actions[1]["reply"]["id"], "appr:r1:deny:mac");
        let body = card["interactive"]["body"]["text"].as_str().unwrap();
        assert!(body.contains("4. deny"), "{body}");
    }
}
