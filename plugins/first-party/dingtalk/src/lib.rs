//! The DingTalk adapter (spec: platform-adapters, DingTalk adapter):
//! Stream Mode, a long-lived outbound WebSocket (held by the socket
//! bridge) opened with a ticket from `/v1.0/gateway/connections/open`, so
//! no public URL is needed. Every callback is acknowledged; pings are
//! echoed.
//!
//! Replies go through the message's session webhook while it is valid
//! ("No session_webhook available" once it expired). With
//! `options.card_template_id` a reply is an AI Card that later edits of
//! the same reply update in place (streaming), instead of new messages.
//! Each admitted message gets a "🤔Thinking" emotion, swapped for
//! "🥳Done" when the reply goes out. Groups need an @mention unless the
//! conversation is in `options.free_response_chats` or the text matches
//! `options.mention_patterns`. Text is cut at 20000 characters.

use std::sync::Mutex;

use adapter_kit::bridge::Bridge;
use adapter_kit::message::{self, Inbound, Outgoing};
use adapter_kit::{Guest, HttpRequest, HttpResponse, clock, host, net, policy, respond};
use serde_json::{Value, json};

const CLIENT_SECRET: &str = "DINGTALK_CLIENT_SECRET";
const MAX_CHARS: usize = 20000;
const TOPIC: &str = "/v1.0/im/bot/messages/get";
const THINKING: &str = "🤔Thinking";
const DONE: &str = "🥳Done";
/// Reconnect waits double from 2 s to this.
const MAX_BACKOFF_SECS: u64 = 60;

/// What a chat's last message left for its reply.
struct Context {
    chat: String,
    webhook: String,
    expires_ms: u64,
    /// The message that carries the Thinking emotion, until Done.
    thinking: Option<String>,
    group: bool,
}

struct DingTalk {
    api: String,
    client_id: String,
    robot_code: String,
    webhook_base: String,
    card_template: Option<String>,
    free_chats: Vec<String>,
    patterns: Vec<String>,
    bridge: Bridge,
    connection: Option<String>,
    backoff_secs: u64,
    retry_at_ms: u64,
    contexts: Vec<Context>,
    /// Open AI cards: (chat, card id).
    cards: Vec<(String, String)>,
    seq: u64,
}

static DT: Mutex<Option<DingTalk>> = Mutex::new(None);

fn secret() -> Result<String, String> {
    host::secret(CLIENT_SECRET)
        .ok_or_else(|| format!("permanent: secret {CLIENT_SECRET} is not set"))
}

/// `text` within 20000 characters.
fn truncate(text: &str) -> String {
    text.chars().take(MAX_CHARS).collect()
}

/// The acknowledgement of a callback or the echo of a ping.
fn ack(headers: &Value, data: &str) -> String {
    json!({
        "code": 200,
        "headers": {"contentType": "application/json", "messageId": headers["messageId"]},
        "message": "OK",
        "data": data,
    })
    .to_string()
}

impl DingTalk {
    fn token(&self) -> Result<String, String> {
        let answer = net::json(
            "POST",
            &format!("{}/v1.0/oauth2/accessToken", self.api),
            &[],
            &json!({"appKey": self.client_id, "appSecret": secret()?}),
        )?;
        answer["accessToken"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "server: DingTalk returned no access token".to_owned())
    }

    fn api(&self, method: &str, path: &str, body: &Value) -> Result<Value, String> {
        let token = self.token()?;
        net::json(
            method,
            &format!("{}{path}", self.api),
            &[("x-acs-dingtalk-access-token", &token)],
            body,
        )
    }

    fn connect(&mut self) -> Result<(), String> {
        let ticket = net::json(
            "POST",
            &format!("{}/v1.0/gateway/connections/open", self.api),
            &[],
            &json!({
                "clientId": self.client_id,
                "clientSecret": secret()?,
                "subscriptions": [{"type": "CALLBACK", "topic": TOPIC}],
                "ua": "prometheus",
            }),
        )?;
        let endpoint = ticket["endpoint"]
            .as_str()
            .ok_or("server: no stream endpoint")?;
        let ticket = ticket["ticket"].as_str().unwrap_or_default();
        let url = format!(
            "{endpoint}?ticket={}",
            adapter_kit::text::percent_encode(ticket)
        );
        self.connection = Some(self.bridge.open(&url, &[])?);
        Ok(())
    }

    fn lost(&mut self) {
        if let Some(id) = self.connection.take() {
            self.bridge.close(&id);
        }
        self.retry_at_ms = clock::now_ms() + self.backoff_secs * 1000;
        self.backoff_secs = (self.backoff_secs * 2).min(MAX_BACKOFF_SECS);
    }

    fn emotion(&self, message: &str, chat: &str, name: &str, recall: bool) {
        let path = if recall {
            "/v1.0/robot/emotion/recall"
        } else {
            "/v1.0/robot/emotion/reply"
        };
        let body = json!({
            "robotCode": self.robot_code,
            "openMsgId": message,
            "openConversationId": chat,
            "emotionType": 2,
            "emotionName": name,
            "textEmotion": {"emotionId": "2659900", "emotionName": name, "text": name, "backgroundId": "im_bg_1"},
        });
        if let Err(why) = self.api("POST", path, &body) {
            host::warn(&format!("DingTalk did not take the {name} emotion: {why}"));
        }
    }

    fn addressed(&self, data: &Value, chat: &str, text: &str) -> bool {
        let lower = text.to_lowercase();
        data["isInAtList"] == true
            || self.free_chats.iter().any(|c| c == chat)
            || self
                .patterns
                .iter()
                .any(|p| lower.contains(&p.to_lowercase()))
    }

    /// One bot message callback: the message to admit, if any.
    fn callback(&mut self, data: &Value) -> Option<Inbound> {
        let chat = data["conversationId"].as_str()?.to_owned();
        let id = data["msgId"].as_str()?.to_owned();
        let sender = data["senderStaffId"]
            .as_str()
            .or(data["senderId"].as_str())?;
        let text = data["text"]["content"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_owned();
        let group = data["conversationType"] == "2";
        if group && !self.addressed(data, &chat, &text) {
            return None;
        }
        let mut inbound = Inbound::text(
            &id,
            &chat,
            if group { "group" } else { "dm" },
            sender,
            &text,
        );
        inbound.sender_name = data["senderNick"].as_str().map(str::to_owned);
        inbound.mentioned = group;
        let webhook = data["sessionWebhook"].as_str().unwrap_or_default();
        if webhook.starts_with(&self.webhook_base) {
            // The webhook's validity, measured on the plugin's own clock.
            let lifetime = data["sessionWebhookExpiredTime"]
                .as_u64()
                .zip(data["createAt"].as_u64())
                .map_or(0, |(end, start)| end.saturating_sub(start));
            self.contexts.retain(|c| c.chat != chat);
            self.contexts.push(Context {
                chat: chat.clone(),
                webhook: webhook.to_owned(),
                expires_ms: clock::now_ms() + lifetime,
                thinking: Some(id.clone()),
                group,
            });
        }
        self.emotion(&id, &chat, THINKING, false);
        Some(inbound)
    }

    fn frame(&mut self, frame: &str, inbound: &mut Vec<Inbound>) -> Result<(), String> {
        let Ok(event) = serde_json::from_str::<Value>(frame) else {
            return Ok(());
        };
        let headers = &event["headers"];
        let id = self.connection.clone().unwrap_or_default();
        match (event["type"].as_str(), headers["topic"].as_str()) {
            (Some("SYSTEM"), Some("ping")) => self
                .bridge
                .send(&id, &ack(headers, event["data"].as_str().unwrap_or("{}"))),
            (Some("CALLBACK"), Some(TOPIC)) => {
                let data: Value = serde_json::from_str(event["data"].as_str().unwrap_or("{}"))
                    .unwrap_or_default();
                inbound.extend(self.callback(&data));
                self.bridge.send(&id, &ack(headers, r#"{"response":null}"#))
            }
            _ => Ok(()),
        }
    }

    fn webhook(&self, chat: &str) -> Result<String, String> {
        self.contexts
            .iter()
            .find(|c| c.chat == chat && clock::now_ms() < c.expires_ms)
            .map(|c| c.webhook.clone())
            .ok_or_else(|| "permanent: No session_webhook available (it expired; DingTalk replies must follow an incoming message)".to_owned())
    }

    /// Swaps the chat's Thinking emotion for Done.
    fn done(&mut self, chat: &str) {
        let Some(message) = self
            .contexts
            .iter_mut()
            .find(|c| c.chat == chat)
            .and_then(|c| c.thinking.take())
        else {
            return;
        };
        self.emotion(&message, chat, THINKING, true);
        self.emotion(&message, chat, DONE, false);
    }

    fn card(&mut self, chat: &str, template: &str, text: &str) -> Result<String, String> {
        self.seq += 1;
        let track = format!("prometheus-card-{}-{}", self.seq, clock::now_ms());
        // A new reply closes the chat's previous card.
        if let Some(at) = self.cards.iter().position(|(c, _)| c == chat) {
            let (_, old) = self.cards.remove(at);
            self.stream(&old, None, true)?;
        }
        let body = json!({
            "cardTemplateId": template,
            "outTrackId": track,
            "openSpaceId": format!("dtv1.card//IM_ROBOT.{chat}"),
            "cardData": {"cardParamMap": {"content": ""}},
            "imRobotOpenDeliverModel": {"spaceType": "IM_ROBOT", "robotCode": self.robot_code},
            "callbackType": "STREAM",
        });
        self.api("POST", "/v1.0/card/instances/createAndDeliver", &body)?;
        self.cards.push((chat.to_owned(), track.clone()));
        self.stream(&track, Some(text), false)?;
        Ok(track)
    }

    fn stream(&self, track: &str, text: Option<&str>, finalize: bool) -> Result<(), String> {
        let body = json!({
            "outTrackId": track,
            "guid": format!("{track}-{}", clock::now_ms()),
            "key": "content",
            "content": text.map(truncate).unwrap_or_default(),
            "isFull": true,
            "isFinalize": finalize,
            "isError": false,
        });
        self.api("PUT", "/v1.0/card/streaming", &body).map(|_| ())
    }

    fn send(&mut self, out: &Outgoing) -> Result<String, String> {
        match out.op.as_str() {
            "" => {}
            "edit" => {
                let track = out.message_id.clone().unwrap_or_default();
                if !self.cards.iter().any(|(_, t)| *t == track) {
                    return Err("unsupported".to_owned());
                }
                self.stream(&track, Some(&out.text), false)?;
                return Ok(track);
            }
            _ => return Err("unsupported".to_owned()),
        }
        let id = match self.card_template.clone() {
            Some(template) => self.card(&out.chat, &template, &out.text)?,
            None => {
                let webhook = self.webhook(&out.chat)?;
                let text = truncate(&out.text);
                let body =
                    json!({"msgtype": "markdown", "markdown": {"title": "Reply", "text": text}});
                let answer = net::json("POST", &webhook, &[], &body)?;
                if answer["errcode"].as_i64().is_some_and(|c| c != 0) {
                    return Err(format!(
                        "permanent: DingTalk refused the reply: {}",
                        answer["errmsg"]
                    ));
                }
                self.seq += 1;
                format!("dt-{}", self.seq)
            }
        };
        self.done(&out.chat);
        Ok(id)
    }
}

fn poll_stream(dt: &mut DingTalk) -> Result<Vec<Inbound>, String> {
    let Some(id) = dt.connection.clone() else {
        if clock::now_ms() >= dt.retry_at_ms && dt.connect().is_err() {
            dt.lost();
        }
        return Ok(Vec::new());
    };
    let received = dt.bridge.recv(&id)?;
    let mut inbound = Vec::new();
    for frame in &received.frames {
        dt.frame(frame, &mut inbound)?;
    }
    if received.open {
        dt.backoff_secs = 2;
    } else {
        dt.lost();
    }
    Ok(inbound)
}

fn with<T>(f: impl FnOnce(&mut DingTalk) -> Result<T, String>) -> Result<T, String> {
    let mut slot = DT.lock().map_err(|e| e.to_string())?;
    f(slot.as_mut().ok_or("not started")?)
}

struct Adapter;

impl Guest for Adapter {
    fn descriptor() -> String {
        json!({
            "platform": "dingtalk",
            "max_message_length": MAX_CHARS,
            "length_unit": "chars",
            "splits_long_messages": false,
            "supports_edit": true,
            "supports_delete": false,
            "supports_typing": false,
            "supports_reactions": false,
            "supports_threads": false,
            "interactive": "numbered",
            "media_in": [],
            "media_out": [],
            "delivery_receipts": false,
            "read_receipts": false,
            "streaming_tts": false,
            "markup": "markdown",
            "binds_http": false,
        })
        .to_string()
    }

    fn start(options: String) -> Result<(), String> {
        let options: Value = serde_json::from_str(&options).map_err(|e| e.to_string())?;
        let client_id = options["client_id"]
            .as_str()
            .filter(|v| !v.is_empty())
            .ok_or("options.client_id is required")?
            .to_owned();
        let base = |key: &str, default: &str| {
            options[key]
                .as_str()
                .unwrap_or(default)
                .trim_end_matches('/')
                .to_owned()
        };
        let mut dt = DingTalk {
            api: base("api_base", "https://api.dingtalk.com"),
            robot_code: options["robot_code"]
                .as_str()
                .unwrap_or(&client_id)
                .to_owned(),
            webhook_base: options["session_webhook_base"]
                .as_str()
                .unwrap_or("https://oapi.dingtalk.com/")
                .to_owned(),
            card_template: options["card_template_id"]
                .as_str()
                .filter(|t| !t.is_empty())
                .map(str::to_owned),
            free_chats: policy::strings(&options, "free_response_chats"),
            patterns: policy::strings(&options, "mention_patterns"),
            bridge: Bridge::from_options(&options)?,
            connection: None,
            backoff_secs: 2,
            retry_at_ms: 0,
            contexts: Vec::new(),
            cards: Vec::new(),
            seq: 0,
            client_id,
        };
        dt.connect()
            .map_err(|e| e.trim_start_matches("permanent: ").to_owned())?;
        let mut slot = DT.lock().map_err(|e| e.to_string())?;
        if let Some(mut old) = slot.take() {
            old.lost();
        }
        *slot = Some(dt);
        Ok(())
    }

    fn stop() {
        if let Ok(mut slot) = DT.lock()
            && let Some(mut dt) = slot.take()
        {
            dt.lost();
        }
    }

    fn poll() -> Result<String, String> {
        clock::tick();
        with(poll_stream).map(|inbound| message::batch(&inbound))
    }

    fn send(message: String) -> Result<String, String> {
        let out = Outgoing::parse(&message)?;
        with(|dt| dt.send(&out))
    }

    fn chat_info(chat: String) -> Result<String, String> {
        // Conversation ids do not tell groups apart: the last message does.
        let group = with(|dt| Ok(dt.contexts.iter().any(|c| c.chat == chat && c.group)))?;
        Ok(message::chat_info(
            &chat,
            if group { "group" } else { "dm" },
        ))
    }

    fn handle_http(_request: HttpRequest) -> HttpResponse {
        respond(404, "text/plain", "")
    }

    fn authorize_sender(_inbound: String) -> String {
        "unknown".to_owned()
    }
}

adapter_kit::bindings::export_adapter!(Adapter);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_callback_is_acknowledged_by_its_message_id() {
        let ack: Value = serde_json::from_str(&ack(&json!({"messageId": "m1"}), "{}")).unwrap();
        assert_eq!(ack["headers"]["messageId"], "m1");
        assert_eq!(ack["code"], 200);
    }

    #[test]
    fn replies_are_cut_at_20000_characters() {
        assert_eq!(truncate(&"é".repeat(25000)).chars().count(), MAX_CHARS);
    }
}
