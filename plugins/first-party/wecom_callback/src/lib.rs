//! The WeCom callback adapter (spec: platform-adapters, WeCom callback
//! adapter): self-built apps post `WXBizMsgCrypt` envelopes to the
//! adapter's shared-listener route. A request is the app's when the SHA1
//! of its token, timestamp, nonce and ciphertext (sorted, concatenated)
//! matches `msg_signature`; the AES-256-CBC payload (key = the 43-character
//! EncodingAESKey, IV = its first 16 bytes) holds 16 random bytes, a
//! big-endian length, the XML and the receiving corp id, which must be
//! the app's. The URL-verification GET answers the decrypted `echostr`;
//! every POST is acknowledged with `success` at once and the reply goes
//! out later through `message/send` with that app's access token.
//!
//! Users are scoped `<corp_id>:<user_id>`, so two apps in two corps never
//! share a chat, and a reply is sent by the app the user wrote to.
//! Text is split by the gateway at 2048 bytes (all `message/send` keeps).

use std::sync::Mutex;

use adapter_kit::message::{self, Inbound, Outgoing};
use adapter_kit::text::xml_field;
use adapter_kit::{
    Guest, HttpRequest, HttpResponse, clock, crypt, host, net, query, respond, sign,
};
use serde_json::{Value, json};

const MAX_APPS: usize = 4;
const MAX_BODY: usize = 64 * 1024;
const MAX_BYTES: usize = 2048;
/// Access tokens live 7200 s; renewed a little before.
const TOKEN_SECS: u64 = 7000;
/// Message ids remembered against WeCom's retries.
const SEEN: usize = 512;

struct App {
    name: String,
    corp_id: String,
    agent_id: i64,
    token: String,
    key: Vec<u8>,
    corp_secret: String,
    access: Option<(String, u64)>,
}

#[derive(Default)]
struct Callback {
    api: String,
    apps: Vec<App>,
    inbox: Vec<Inbound>,
    seen: Vec<String>,
    /// chat -> the app (index) its user last wrote through.
    routes: Vec<(String, usize)>,
}

static STATE: Mutex<Option<Callback>> = Mutex::new(None);

fn signature(token: &str, timestamp: &str, nonce: &str, encrypt: &str) -> String {
    let mut parts = [token, timestamp, nonce, encrypt];
    parts.sort_unstable();
    sign::hex(&sign::sha1(parts.concat().as_bytes()))
}

impl App {
    /// Opens `encrypt` when `msg_signature` is this app's: the XML.
    fn open(
        &self,
        msg_signature: &str,
        timestamp: &str,
        nonce: &str,
        encrypt: &str,
    ) -> Option<String> {
        let expected = signature(&self.token, timestamp, nonce, encrypt);
        if !sign::same(expected.as_bytes(), msg_signature.as_bytes()) {
            return None;
        }
        let sealed = sign::base64_decode(encrypt).ok()?;
        let plain = crypt::aes256_cbc_decrypt(&self.key, &self.key[..16], &sealed).ok()?;
        let plain = crypt::unpad(plain, 32).ok()?;
        let body = plain.get(16..)?;
        let len = usize::try_from(u32::from_be_bytes(body.get(..4)?.try_into().ok()?)).ok()?;
        let xml = body.get(4..4 + len)?;
        let receiver = body.get(4 + len..)?;
        (receiver == self.corp_id.as_bytes()).then(|| String::from_utf8_lossy(xml).into_owned())
    }
}

impl Callback {
    fn token(&mut self, index: usize, fresh: bool) -> Result<String, String> {
        let api = self.api.clone();
        let app = &mut self.apps[index];
        if !fresh
            && let Some((token, at)) = &app.access
            && !clock::elapsed(*at, TOKEN_SECS)
        {
            return Ok(token.clone());
        }
        let url = format!(
            "{api}/cgi-bin/gettoken?corpid={}&corpsecret={}",
            adapter_kit::text::percent_encode(&app.corp_id),
            adapter_kit::text::percent_encode(&app.corp_secret)
        );
        let answer = net::get(&url, &[])?;
        let token = answer["access_token"].as_str().ok_or_else(|| {
            format!(
                "permanent: WeCom refused app '{}' a token: {}",
                app.name, answer["errmsg"]
            )
        })?;
        app.access = Some((token.to_owned(), clock::now_ms()));
        Ok(token.to_owned())
    }

    /// The app a reply to `chat` goes through: the one its user wrote
    /// to, else the only app of its corp.
    fn app_for(&self, chat: &str) -> Result<usize, String> {
        if let Some((_, index)) = self.routes.iter().find(|(c, _)| c == chat) {
            return Ok(*index);
        }
        let corp = chat.split_once(':').map_or("", |(c, _)| c);
        let mut of_corp = self
            .apps
            .iter()
            .enumerate()
            .filter(|(_, a)| a.corp_id == corp);
        match (of_corp.next(), of_corp.next()) {
            (Some((index, _)), None) => Ok(index),
            _ => Err(format!(
                "permanent: no WeCom app for chat '{chat}' (chats are <corp_id>:<user_id>)"
            )),
        }
    }

    fn send(&mut self, out: &Outgoing) -> Result<String, String> {
        if !out.op.is_empty() {
            return Err("unsupported".to_owned());
        }
        let index = self.app_for(&out.chat)?;
        let user = out
            .chat
            .split_once(':')
            .map_or(out.chat.as_str(), |(_, u)| u);
        let body = json!({
            "touser": user,
            "msgtype": "text",
            "agentid": self.apps[index].agent_id,
            "text": {"content": out.text},
            "safe": 0,
        });
        for attempt in 0..2 {
            let token = self.token(index, attempt > 0)?;
            let url = format!("{}/cgi-bin/message/send?access_token={token}", self.api);
            let answer = net::json("POST", &url, &[], &body)?;
            match answer["errcode"].as_i64().unwrap_or(0) {
                0 => return Ok(answer["msgid"].as_str().unwrap_or("sent").to_owned()),
                // A token WeCom no longer takes: fetch a fresh one once.
                40001 | 42001 if attempt == 0 => {}
                45009 => return Err("rate_limited".to_owned()),
                code => {
                    return Err(format!(
                        "permanent: WeCom message/send failed ({code}): {}",
                        answer["errmsg"]
                    ));
                }
            }
        }
        Err("server: WeCom kept refusing fresh access tokens".to_owned())
    }

    /// The message an opened callback carries, if it is one to admit.
    fn admit(&mut self, index: usize, xml: &str) {
        let kind = xml_field(xml, "MsgType").unwrap_or_default().to_lowercase();
        let event = xml_field(xml, "Event").unwrap_or_default().to_lowercase();
        if kind != "text" && kind != "event"
            || matches!(event.as_str(), "enter_agent" | "subscribe")
        {
            return;
        }
        let user = xml_field(xml, "FromUserName").unwrap_or_default();
        if user.is_empty() {
            return;
        }
        let corp = xml_field(xml, "ToUserName").unwrap_or_else(|| self.apps[index].corp_id.clone());
        let id = xml_field(xml, "MsgId").unwrap_or_else(|| {
            format!(
                "{user}:{}",
                xml_field(xml, "CreateTime").unwrap_or_default()
            )
        });
        if self.seen.contains(&id) {
            return;
        }
        self.seen.push(id.clone());
        if self.seen.len() > SEEN {
            self.seen.remove(0);
        }
        let mut text = xml_field(xml, "Content")
            .unwrap_or_default()
            .trim()
            .to_owned();
        if text.is_empty() && kind == "event" {
            "/start".clone_into(&mut text);
        }
        let chat = format!("{corp}:{user}");
        self.routes.retain(|(c, _)| *c != chat);
        self.routes.push((chat.clone(), index));
        let mut inbound = Inbound::text(&id, &chat, "dm", &chat, &text);
        inbound.sender_name = Some(user);
        self.inbox.push(inbound);
    }

    fn verify(&self, request: &HttpRequest) -> HttpResponse {
        let params = Params::of(&request.path);
        let echo = params.get("echostr");
        for app in &self.apps {
            if let Some(plain) =
                app.open(&params.signature, &params.timestamp, &params.nonce, &echo)
            {
                return respond(200, "text/plain", plain);
            }
        }
        respond(403, "text/plain", "signature verification failed")
    }

    fn callback(&mut self, request: &HttpRequest) -> HttpResponse {
        if request.body.len() > MAX_BODY {
            return respond(413, "text/plain", "payload too large");
        }
        let params = Params::of(&request.path);
        let body = String::from_utf8_lossy(&request.body);
        let Some(encrypt) = xml_field(&body, "Encrypt") else {
            return respond(400, "text/plain", "invalid callback payload");
        };
        let opened = self.apps.iter().enumerate().find_map(|(index, app)| {
            app.open(
                &params.signature,
                &params.timestamp,
                &params.nonce,
                &encrypt,
            )
            .map(|xml| (index, xml))
        });
        match opened {
            Some((index, xml)) => {
                self.admit(index, &xml);
                respond(200, "text/plain", "success")
            }
            None => respond(401, "text/plain", "signature verification failed"),
        }
    }
}

/// The signature parameters of a callback's query.
struct Params {
    pairs: Vec<(String, String)>,
    signature: String,
    timestamp: String,
    nonce: String,
}

impl Params {
    fn of(path: &str) -> Self {
        let pairs = query(path);
        let get = |k: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        Self {
            signature: get("msg_signature"),
            timestamp: get("timestamp"),
            nonce: get("nonce"),
            pairs: pairs.clone(),
        }
    }

    fn get(&self, key: &str) -> String {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }
}

fn app_of(n: usize, options: &Value) -> Result<App, String> {
    let secret = |kind: &str| {
        let name = format!("WECOM_CALLBACK_{kind}_{n}");
        host::secret(&name).ok_or(format!("secret {name} is not set"))
    };
    let corp_id = options["corp_id"]
        .as_str()
        .filter(|c| !c.is_empty())
        .ok_or(format!("options.apps[{}].corp_id is required", n - 1))?
        .to_owned();
    let aes_key = secret("AES_KEY")?;
    if aes_key.len() != 43 {
        return Err(format!(
            "secret WECOM_CALLBACK_AES_KEY_{n} must be the 43-character EncodingAESKey"
        ));
    }
    let key = sign::base64_decode(&format!("{aes_key}="))
        .ok()
        .filter(|k| k.len() == 32)
        .ok_or(format!(
            "secret WECOM_CALLBACK_AES_KEY_{n} is not a valid EncodingAESKey"
        ))?;
    Ok(App {
        name: options["name"].as_str().unwrap_or(&corp_id).to_owned(),
        agent_id: options["agent_id"]
            .as_i64()
            .or_else(|| options["agent_id"].as_str().and_then(|a| a.parse().ok()))
            .ok_or(format!("options.apps[{}].agent_id is required", n - 1))?,
        token: secret("TOKEN")?,
        corp_secret: secret("CORP_SECRET")?,
        key,
        corp_id,
        access: None,
    })
}

fn with<T>(f: impl FnOnce(&mut Callback) -> T) -> Result<T, String> {
    let mut slot = STATE.lock().map_err(|e| e.to_string())?;
    Ok(f(slot.as_mut().ok_or("not started")?))
}

struct Adapter;

impl Guest for Adapter {
    fn descriptor() -> String {
        json!({
            "platform": "wecom_callback",
            "max_message_length": MAX_BYTES,
            "length_unit": "utf8_bytes",
            "splits_long_messages": false,
            "supports_edit": false,
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
            "markup": "plain",
            "binds_http": true,
        })
        .to_string()
    }

    fn start(options: String) -> Result<(), String> {
        let options: Value = serde_json::from_str(&options).map_err(|e| e.to_string())?;
        let listed = options["apps"].as_array().cloned().unwrap_or_default();
        if listed.is_empty() {
            return Err(
                "options.apps must list at least one app {name, corp_id, agent_id}".to_owned(),
            );
        }
        if listed.len() > MAX_APPS {
            return Err(format!("options.apps holds at most {MAX_APPS} apps"));
        }
        let apps = listed
            .iter()
            .enumerate()
            .map(|(i, app)| app_of(i + 1, app))
            .collect::<Result<Vec<_>, _>>()?;
        let api = options["api_base"]
            .as_str()
            .unwrap_or("https://qyapi.weixin.qq.com")
            .trim_end_matches('/')
            .to_owned();
        *STATE.lock().map_err(|e| e.to_string())? = Some(Callback {
            api,
            apps,
            ..Callback::default()
        });
        Ok(())
    }

    fn stop() {
        if let Ok(mut slot) = STATE.lock() {
            *slot = None;
        }
    }

    fn poll() -> Result<String, String> {
        clock::tick();
        with(|state| message::batch(&std::mem::take(&mut state.inbox)))
    }

    fn send(message: String) -> Result<String, String> {
        let out = Outgoing::parse(&message)?;
        with(|state| state.send(&out))?
    }

    fn chat_info(chat: String) -> Result<String, String> {
        Ok(message::chat_info(&chat, "dm"))
    }

    fn handle_http(request: HttpRequest) -> HttpResponse {
        let answer = with(|state| match request.method.as_str() {
            "GET" => state.verify(&request),
            "POST" => state.callback(&request),
            _ => respond(405, "text/plain", ""),
        });
        answer.unwrap_or_else(|_| respond(503, "text/plain", "not started"))
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
    fn the_signature_sorts_its_parts() {
        // WXBizMsgCrypt: sha1 of the sorted strings concatenated.
        assert_eq!(
            signature("b", "a", "d", "c"),
            sign::hex(&sign::sha1(b"abcd"))
        );
    }
}
