//! Sends through the Chat API as the app: text (in the thread asked for),
//! choices as a Card v2 with one button per choice (a clarify question
//! also gets "Other"), the working-state message ("Prometheus is
//! thinking…", unless `typing_indicator = false`) that the reply then
//! replaces in place, edits, deletes, and files. Files go up natively with
//! the recipient's own `/setup-files` token (an app cannot upload); a user
//! without one gets a notice saying how to enable it.

use adapter_kit::message::{Button, OutFile, Outgoing};
use adapter_kit::net;
use serde_json::{Value, json};

use crate::inbound::OTHER;
use crate::{GoogleChat, THINKING, thread_key};

const BOUNDARY: &str = "prometheus-google-chat-upload";

/// The Card v2 of `text` with `buttons`.
pub(crate) fn card(text: &str, buttons: &[Button]) -> Value {
    let mut actions: Vec<Value> = buttons.iter().map(|b| button(&b.label, &b.data)).collect();
    if buttons.iter().any(|b| b.data.starts_with("clar:")) {
        actions.push(button("Other", OTHER));
    }
    json!([{
        "cardId": "prometheus-choice",
        "card": {"sections": [{"widgets": [
            {"textParagraph": {"text": text}},
            {"buttonList": {"buttons": actions}},
        ]}]},
    }])
}

fn button(label: &str, data: &str) -> Value {
    json!({
        "text": label,
        "onClick": {"action": {"function": "prometheus", "parameters": [{"key": "data", "value": data}]}},
    })
}

/// A `multipart/related` upload body: the metadata, then the file.
fn related(file: &OutFile) -> Vec<u8> {
    let mut body = format!(
        "--{BOUNDARY}\r\ncontent-type: application/json; charset=UTF-8\r\n\r\n{}\r\n--{BOUNDARY}\r\ncontent-type: {}\r\n\r\n",
        json!({"filename": file.name}),
        file.mime
    )
    .into_bytes();
    body.extend_from_slice(&file.data);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

impl GoogleChat {
    fn call(
        &mut self,
        method: &str,
        path: &str,
        bearer: &str,
        body: Option<&Value>,
    ) -> Result<Value, String> {
        let url = format!("{}/v1/{path}", self.auth.urls.chat);
        let mut headers = vec![("authorization", bearer)];
        if body.is_some() {
            headers.push(("content-type", "application/json"));
        }
        let response = net::send(
            method,
            &url,
            &headers,
            body.map(|b| b.to_string().into_bytes()),
        )?;
        net::classify(&response)?;
        Ok(response.json())
    }

    /// Creates a message in `space` (and `thread`): its name.
    pub(crate) fn create(
        &mut self,
        space: &str,
        thread: Option<&str>,
        body: &Value,
    ) -> Result<String, String> {
        let bearer = self.auth.bot()?;
        self.create_as(&bearer, space, thread, body)
    }

    fn create_as(
        &mut self,
        bearer: &str,
        space: &str,
        thread: Option<&str>,
        body: &Value,
    ) -> Result<String, String> {
        let mut body = body.clone();
        let mut path = format!("{space}/messages");
        if let Some(thread) = thread {
            body["thread"] = json!({"name": thread});
            path.push_str("?messageReplyOption=REPLY_MESSAGE_FALLBACK_TO_NEW_THREAD");
        }
        let created = self.call("POST", &path, bearer, Some(&body))?;
        Ok(created["name"].as_str().unwrap_or_default().to_owned())
    }

    fn patch(&mut self, name: &str, body: &Value, mask: &str) -> Result<(), String> {
        let bearer = self.auth.bot()?;
        self.call(
            "PATCH",
            &format!("{name}?updateMask={mask}"),
            &bearer,
            Some(body),
        )
        .map(|_| ())
    }

    /// Sends `out`: its files, then its text or card; the message name.
    pub(crate) fn send(&mut self, out: &Outgoing) -> Result<String, String> {
        let thread = out.thread.as_deref();
        let mut last = String::new();
        for file in &out.attachments {
            last = self.send_file(&out.chat, thread, file)?;
        }
        if !out.buttons.is_empty() {
            let body = json!({"text": "", "cardsV2": card(&out.text, &out.buttons)});
            let name = self.create(&out.chat, thread, &body)?;
            self.cards.push(name.clone());
            return Ok(name);
        }
        if out.text.trim().is_empty() {
            return Ok(last);
        }
        let body = json!({"text": out.text});
        // The reply replaces the working-state message where there is one.
        if let Some(name) = self.thinking.remove(&thread_key(&out.chat, thread)) {
            self.patch(&name, &body, "text")?;
            return Ok(name);
        }
        self.create(&out.chat, thread, &body)
    }

    pub(crate) fn edit(&mut self, out: &Outgoing) -> Result<(), String> {
        let name = out.message_id.clone().ok_or("permanent: no message id")?;
        if out.buttons.is_empty() && !self.cards.contains(&name) {
            return self.patch(&name, &json!({"text": out.text}), "text");
        }
        let cards = if out.buttons.is_empty() {
            // The decision replaces the card's choices.
            card(&out.text, &[])
        } else {
            card(&out.text, &out.buttons)
        };
        self.patch(
            &name,
            &json!({"text": "", "cardsV2": cards}),
            "text,cardsV2",
        )
    }

    pub(crate) fn delete(&mut self, out: &Outgoing) -> Result<(), String> {
        let name = out.message_id.clone().ok_or("permanent: no message id")?;
        let bearer = self.auth.bot()?;
        self.call("DELETE", &name, &bearer, None).map(|_| ())
    }

    /// Shows the working-state message (once per turn).
    pub(crate) fn typing(&mut self, out: &Outgoing) -> Result<(), String> {
        let key = thread_key(&out.chat, out.thread.as_deref());
        if !self.settings.typing_indicator || self.thinking.contains_key(&key) {
            return Ok(());
        }
        let name = self.create(&out.chat, out.thread.as_deref(), &json!({"text": THINKING}))?;
        self.thinking.insert(key, name);
        Ok(())
    }

    /// Uploads `file` with the recipient's own token and posts it as them;
    /// without a token, a notice.
    fn send_file(
        &mut self,
        space: &str,
        thread: Option<&str>,
        file: &OutFile,
    ) -> Result<String, String> {
        let recipient = self.senders.get(space).cloned().unwrap_or_default();
        let client = self.settings.oauth_client_id.clone().unwrap_or_default();
        let user = if recipient.is_empty() || client.is_empty() {
            None
        } else {
            self.auth.user(&recipient, &client)?
        };
        let Some(bearer) = user else {
            let notice = format!(
                "I have a file for you ({}), but Google Chat lets me attach files only with your own authorization: send /setup-files start to me once.",
                file.name
            );
            return self.create(space, thread, &json!({"text": notice}));
        };
        let url = format!(
            "{}/upload/v1/{space}/attachments:upload?uploadType=multipart",
            self.auth.urls.chat
        );
        let content_type = format!("multipart/related; boundary={BOUNDARY}");
        let response = net::send(
            "POST",
            &url,
            &[("authorization", &bearer), ("content-type", &content_type)],
            Some(related(file)),
        )?;
        net::classify(&response)?;
        let reference = response.json()["attachmentDataRef"].clone();
        if reference.is_null() {
            return Err("server: the upload returned no attachmentDataRef".to_owned());
        }
        let body = json!({"attachment": [{"attachmentDataRef": reference}]});
        // The reference is bound to the uploader: the user posts it.
        self.create_as(&bearer, space, thread, &body)
    }

    pub(crate) fn chat_info(&mut self, chat: &str) -> Result<String, String> {
        let bearer = self.auth.bot()?;
        let space = self.call("GET", chat, &bearer, None)?;
        let dm = space["spaceType"] == "DIRECT_MESSAGE" || space["type"] == "DM";
        let name = space["displayName"].as_str().unwrap_or(chat);
        Ok(adapter_kit::message::chat_info(
            name,
            if dm { "dm" } else { "group" },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clarify_card_adds_other() {
        let choice = |n: u8| Button {
            label: format!("Choice {n}"),
            data: format!("clar:q:0.{n}:mac"),
        };
        let cards = card("Which?", &[choice(1), choice(2)]);
        let buttons = cards[0]["card"]["sections"][0]["widgets"][1]["buttonList"]["buttons"]
            .as_array()
            .unwrap();
        assert_eq!(buttons.len(), 3);
        assert_eq!(buttons[2]["text"], "Other");
        assert_eq!(
            buttons[0]["onClick"]["action"]["parameters"][0]["value"],
            "clar:q:0.1:mac"
        );
        let approval = card(
            "Approve?",
            &[Button {
                label: "Once".into(),
                data: "appr:r:once:m".into(),
            }],
        );
        let only = approval[0]["card"]["sections"][0]["widgets"][1]["buttonList"]["buttons"]
            .as_array()
            .unwrap();
        assert_eq!(only.len(), 1);
    }
}
