//! Webhook messages to inbound ones: text, button and list taps (their id
//! is the choice's data), numbered answers to a pending choice message,
//! and media downloaded through the Graph API (`GET /<media id>` names a
//! short-lived URL, fetched with the same token). Each message is marked
//! read with a typing indicator while the agent works on it.

use adapter_kit::message::{InFile, Inbound};
use adapter_kit::{host, net};
use serde_json::{Value, json};

use crate::{Settings, State, auth};

/// Queues the messages of a webhook body that belong to this phone
/// number, with their senders' profile names.
pub(crate) fn accept(state: &mut State, body: &Value) {
    let Some(settings) = state.settings.clone() else {
        return;
    };
    let changes = body["entry"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|e| e["changes"].as_array().into_iter().flatten());
    for change in changes {
        let value = &change["value"];
        let number = value["metadata"]["phone_number_id"].as_str();
        if change["field"] != "messages" || number != Some(settings.phone_number_id.as_str()) {
            continue;
        }
        let name_of = |wa_id: &str| {
            value["contacts"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|c| c["wa_id"] == wa_id)
                .and_then(|c| c["profile"]["name"].as_str())
                .map(str::to_owned)
        };
        for message in value["messages"].as_array().into_iter().flatten() {
            let name = name_of(message["from"].as_str().unwrap_or_default());
            state.raw.push((message.clone(), name));
        }
    }
}

/// The text and pressed choice of a message.
fn text_of(message: &Value) -> (String, Option<String>) {
    let field = |v: &Value| v.as_str().unwrap_or_default().to_owned();
    match message["type"].as_str().unwrap_or_default() {
        "text" => (field(&message["text"]["body"]), None),
        "interactive" => {
            let inner = &message["interactive"];
            let reply = if inner["type"] == "list_reply" {
                &inner["list_reply"]
            } else {
                &inner["button_reply"]
            };
            (
                field(&reply["title"]),
                reply["id"].as_str().map(str::to_owned),
            )
        }
        "button" => (field(&message["button"]["text"]), None),
        kind => (field(&message[kind]["caption"]), None),
    }
}

/// The inbound message of a webhook message (`None` for kinds it does not
/// carry, such as reactions).
pub(crate) fn normalize(
    settings: &Settings,
    message: &Value,
    name: Option<String>,
) -> Option<Inbound> {
    let from = message["from"].as_str()?;
    let id = message["id"].as_str()?;
    let kind = message["type"].as_str().unwrap_or_default();
    if matches!(kind, "reaction" | "unsupported" | "system") {
        return None;
    }
    let (text, callback) = text_of(message);
    let mut inbound = Inbound::text(id, from, "dm", from, &text);
    inbound.sender_name = name;
    inbound.callback = callback;
    inbound.reply_to = message["context"]["id"].as_str().map(str::to_owned);
    if let Some(file) = media(settings, message) {
        inbound.attachments.push(file);
    }
    if let Err(why) = read_with_typing(settings, id) {
        host::warn(&format!("WhatsApp did not take the read mark: {why}"));
    }
    Some(inbound)
}

/// Marks `message_id` read and shows the typing indicator.
pub(crate) fn read_with_typing(settings: &Settings, message_id: &str) -> Result<(), String> {
    let url = format!("{}/{}/messages", settings.graph, settings.phone_number_id);
    let body = json!({
        "messaging_product": "whatsapp",
        "status": "read",
        "message_id": message_id,
        "typing_indicator": {"type": "text"},
    });
    net::json("POST", &url, &[("authorization", &auth()?)], &body).map(|_| ())
}

/// The file of a media message, downloaded.
fn media(settings: &Settings, message: &Value) -> Option<InFile> {
    let kind = message["type"].as_str()?;
    let block = &message[kind];
    let ours = match kind {
        "image" | "sticker" => "image",
        "audio" if block["voice"] == true => "voice",
        "audio" => "audio",
        "video" => "video",
        "document" => "document",
        _ => return None,
    };
    let media_id = block["id"].as_str()?;
    let name = block["filename"].as_str().map_or_else(
        || format!("{media_id}.{}", extension(block["mime_type"].as_str())),
        str::to_owned,
    );
    match download(settings, media_id) {
        Ok(bytes) => Some(InFile::bytes(ours, &name, bytes)),
        Err(why) => {
            host::warn(&format!(
                "could not download WhatsApp media {media_id}: {why}"
            ));
            None
        }
    }
}

fn extension(mime: Option<&str>) -> &'static str {
    match mime
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
    {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "audio/ogg" => "ogg",
        "audio/mpeg" => "mp3",
        "audio/mp4" | "audio/aac" => "m4a",
        "video/mp4" => "mp4",
        "application/pdf" => "pdf",
        _ => "bin",
    }
}

fn download(settings: &Settings, media_id: &str) -> Result<Vec<u8>, String> {
    let auth = auth()?;
    let meta = net::get(
        &format!("{}/{media_id}", settings.graph),
        &[("authorization", &auth)],
    )?;
    let url = meta["url"].as_str().ok_or("Graph named no media URL")?;
    let response = net::send("GET", url, &[("authorization", &auth)], None)?;
    net::classify(&response)?;
    Ok(response.body)
}

/// Records the chat's latest message and maps a numbered answer (or a
/// tap) to the pending choice message it answers.
pub(crate) fn answer_choice(state: &mut State, inbound: &mut Inbound) {
    state.last.insert(inbound.chat.clone(), inbound.id.clone());
    let pending = state.choices.iter().rposition(|c| c.chat == inbound.chat);
    let Some(index) = pending else {
        return;
    };
    if let Some(callback) = &inbound.callback {
        if state.choices[index].data.contains(callback) {
            state.choices.remove(index);
        }
        return;
    }
    let number = inbound.text.trim().parse::<usize>().ok();
    let data = number
        .filter(|n| *n >= 1)
        .and_then(|n| state.choices[index].data.get(n - 1).cloned());
    if let Some(data) = data {
        inbound.callback = Some(data);
        state.choices.remove(index);
    }
}
