//! Chat events to inbound messages. A `MESSAGE` from a human becomes one
//! (`/setup-files` is answered here and never reaches the agent); a
//! `CARD_CLICKED` becomes a button press (the "Other" button asks for a
//! typed answer). Spaces keep one session per thread; a DM shares one
//! session except in a thread the user replied in. Users are their
//! emails. Attachments are downloaded through the Chat media API with the
//! app's token, which goes only to Google hosts.

use adapter_kit::message::{InFile, Inbound, kind_of_name};
use adapter_kit::{host, net, sign};
use serde_json::{Value, json};

use crate::{GoogleChat, Mode};

/// What the "Other" choice button carries.
pub(crate) const OTHER: &str = "prometheus:other";

fn is_dm(space: &Value) -> bool {
    space["type"] == "DM"
        || space["spaceType"] == "DIRECT_MESSAGE"
        || space["singleUserBotDm"] == true
}

/// The message and space of an event (native or Workspace add-on form).
fn payload(event: &Value) -> Option<(&Value, &Value)> {
    if let Some(wrapped) = event["chat"]["messagePayload"].as_object() {
        let message = wrapped.get("message")?;
        let space = wrapped.get("space").unwrap_or(&message["space"]);
        return Some((message, space));
    }
    if event["type"] != "MESSAGE" {
        return None;
    }
    let message = event.get("message")?;
    let space = event
        .get("space")
        .filter(|s| s.is_object())
        .unwrap_or(&message["space"]);
    Some((message, space))
}

/// A card click's button data.
fn clicked(event: &Value) -> Option<String> {
    if let Some(data) = event["common"]["parameters"]["data"].as_str() {
        return Some(data.to_owned());
    }
    event["action"]["parameters"]
        .as_array()?
        .iter()
        .find(|p| p["key"] == "data")
        .and_then(|p| p["value"].as_str())
        .map(str::to_owned)
}

impl GoogleChat {
    /// Takes one event.
    pub(crate) fn event(&mut self, event: &Value) {
        if event["type"] == "CARD_CLICKED" {
            self.click(event);
        } else if let Some((message, space)) = payload(event) {
            self.message(message, space);
        }
    }

    fn message(&mut self, message: &Value, space: &Value) {
        let sender = &message["sender"];
        if sender["type"] == "BOT" {
            return;
        }
        let Some(chat) = space["name"].as_str().filter(|n| !n.is_empty()) else {
            return;
        };
        let email = sender["email"].as_str().unwrap_or_default().to_lowercase();
        let id = if email.is_empty() {
            sender["name"].as_str().unwrap_or_default().to_owned()
        } else {
            email.clone()
        };
        let text = message["argumentText"]
            .as_str()
            .or_else(|| message["text"].as_str())
            .unwrap_or_default()
            .trim()
            .to_owned();
        let dm = is_dm(space);
        let thread = message["thread"]["name"]
            .as_str()
            .filter(|_| !dm || message["threadReply"] == true)
            .map(str::to_owned);
        if !email.is_empty() {
            self.senders.insert(chat.to_owned(), email.clone());
        }
        if let Some(arg) = text.strip_prefix("/setup-files")
            && (arg.is_empty() || arg.starts_with(' '))
        {
            self.setup_files(chat, thread.as_deref(), &email, arg.trim());
            return;
        }
        let name = message["name"].as_str().unwrap_or_default();
        let kind = if dm { "dm" } else { "group" };
        let mut inbound = Inbound::text(name, chat, kind, &id, &text);
        inbound.sender_name = sender["displayName"].as_str().map(str::to_owned);
        inbound.thread = thread;
        // In a space Chat only delivers messages that @mention the app.
        inbound.mentioned = !dm;
        for attachment in message["attachment"].as_array().into_iter().flatten() {
            if let Some(file) = self.download(attachment) {
                inbound.attachments.push(file);
            }
        }
        self.inbox.push(inbound);
    }

    fn click(&mut self, event: &Value) {
        let Some(data) = clicked(event) else {
            return;
        };
        let space = &event["space"];
        let Some(chat) = space["name"].as_str() else {
            return;
        };
        let thread = event["message"]["thread"]["name"]
            .as_str()
            .filter(|_| !is_dm(space) || event["message"]["threadReply"] == true)
            .map(str::to_owned);
        if data == OTHER {
            let note = json!({"text": "Type your answer as a message."});
            if let Err(why) = self.create(chat, thread.as_deref(), &note) {
                host::warn(&format!("could not ask for a typed answer: {why}"));
            }
            return;
        }
        let user = &event["user"];
        let sender = user["email"]
            .as_str()
            .map(str::to_lowercase)
            .or_else(|| user["name"].as_str().map(str::to_owned))
            .unwrap_or_default();
        self.clicks += 1;
        let id = format!(
            "click:{}:{}",
            event["message"]["name"].as_str().unwrap_or_default(),
            self.clicks
        );
        let kind = if is_dm(space) { "dm" } else { "group" };
        let mut inbound = Inbound::text(&id, chat, kind, &sender, "");
        inbound.callback = Some(data);
        inbound.thread = thread;
        self.inbox.push(inbound);
    }

    fn download(&mut self, attachment: &Value) -> Option<InFile> {
        let name = attachment["contentName"]
            .as_str()
            .unwrap_or("attachment")
            .to_owned();
        let url = match attachment["attachmentDataRef"]["resourceName"].as_str() {
            Some(resource) => format!("{}/v1/media/{resource}?alt=media", self.auth.urls.chat),
            None => attachment["downloadUri"].as_str()?.to_owned(),
        };
        if !self.auth.urls.trusted(&url) {
            host::warn(&format!(
                "not downloading {name}: {url} is not a Google host"
            ));
            return None;
        }
        let bearer = self.auth.bot().ok()?;
        match net::send("GET", &url, &[("authorization", &bearer)], None) {
            Ok(response) if response.status < 300 => {
                let kind = match attachment["contentType"].as_str().unwrap_or_default() {
                    t if t.starts_with("image/") => "image",
                    t if t.starts_with("audio/") => "audio",
                    t if t.starts_with("video/") => "video",
                    _ => kind_of_name(&name),
                };
                Some(InFile::bytes(kind, &name, response.body))
            }
            _ => {
                host::warn(&format!(
                    "could not download the Google Chat attachment {name}"
                ));
                None
            }
        }
    }

    /// Pulls the subscription once and acknowledges what it got.
    pub(crate) fn pull(&mut self) -> Result<(), String> {
        let Mode::PubSub { subscription, max } = self.settings.mode.clone() else {
            return Ok(());
        };
        let bearer = self.auth.bot()?;
        let url = format!("{}/v1/{subscription}:pull", self.auth.urls.pubsub);
        let body = json!({"maxMessages": max, "returnImmediately": true});
        let pulled = net::json("POST", &url, &[("authorization", &bearer)], &body)?;
        let mut acks = Vec::new();
        for received in pulled["receivedMessages"].as_array().into_iter().flatten() {
            if let Some(ack) = received["ackId"].as_str() {
                acks.push(ack.to_owned());
            }
            let data = received["message"]["data"].as_str().unwrap_or_default();
            match sign::base64_decode(data)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            {
                Some(event) => self.event(&event),
                None => host::warn("a Pub/Sub message held no Chat event"),
            }
        }
        if !acks.is_empty() {
            let url = format!("{}/v1/{subscription}:acknowledge", self.auth.urls.pubsub);
            net::json(
                "POST",
                &url,
                &[("authorization", &bearer)],
                &json!({"ackIds": acks}),
            )?;
        }
        Ok(())
    }
}
