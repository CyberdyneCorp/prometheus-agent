//! The SMS adapter (spec: platform-adapters, SMS adapter): Twilio
//! webhooks in, verified by `X-Twilio-Signature` (HMAC-SHA1 with the auth
//! token over `options.public_url` followed by the sorted form
//! parameters, each name then value; a mismatch is 401), and plain-text
//! replies out through the Messages API with Markdown stripped. Messages
//! from the adapter's own number are ignored, phone numbers are redacted
//! in logs, and choices render as a numbered list (descriptor
//! `interactive = "numbered"`). Unknown senders fall to the gateway's
//! default deny.

use std::sync::Mutex;

use adapter_kit::message::{self, Inbound, Outgoing};
use adapter_kit::{Guest, HttpRequest, HttpResponse, host, net, respond, sign, text};
use serde_json::{Value, json};

const AUTH_TOKEN: &str = "TWILIO_AUTH_TOKEN";

/// What `start` read.
#[derive(Default)]
struct Settings {
    api: String,
    account_sid: String,
    phone_number: String,
    public_url: String,
}

static SETTINGS: Mutex<Option<Settings>> = Mutex::new(None);
/// Verified webhooks waiting for the next poll.
static INBOX: Mutex<Vec<Inbound>> = Mutex::new(Vec::new());

fn required(options: &Value, key: &str) -> Result<String, String> {
    options[key]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("options.{key} is required"))
}

/// `+15551234567` as `+1555***4567` for logs.
fn redact(number: &str) -> String {
    let chars: Vec<char> = number.chars().collect();
    if chars.len() <= 6 {
        return "***".to_owned();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}***{tail}")
}

/// Twilio's signature of a form POST to `url`.
fn signature(token: &str, url: &str, params: &[(String, String)]) -> String {
    let mut sorted: Vec<&(String, String)> = params.iter().collect();
    sorted.sort();
    let mut data = url.to_owned();
    for (name, value) in sorted {
        data.push_str(name);
        data.push_str(value);
    }
    sign::base64(&sign::hmac_sha1(token.as_bytes(), data.as_bytes()))
}

fn with_settings<T>(f: impl FnOnce(&Settings) -> T) -> Result<T, String> {
    let guard = SETTINGS.lock().map_err(|e| e.to_string())?;
    guard
        .as_ref()
        .map(f)
        .ok_or_else(|| "not started".to_owned())
}

fn receive(request: &HttpRequest) -> HttpResponse {
    let Some(token) = host::secret(AUTH_TOKEN) else {
        return respond(503, "text/plain", "TWILIO_AUTH_TOKEN is not set");
    };
    let body = String::from_utf8_lossy(&request.body);
    let params = text::form_decode(&body);
    let Ok((url, own)) = with_settings(|s| (s.public_url.clone(), s.phone_number.clone())) else {
        return respond(503, "text/plain", "not started");
    };
    let expected = signature(&token, &url, &params);
    let given = adapter_kit::header(request, "x-twilio-signature").unwrap_or_default();
    if !sign::same(expected.as_bytes(), given.as_bytes()) {
        return respond(401, "text/plain", "bad signature");
    }
    let field = |name: &str| {
        params
            .iter()
            .find(|(k, _)| k == name)
            .map_or("", |(_, v)| v.as_str())
    };
    let from = field("From");
    if from == own {
        host::info("ignored an SMS from the adapter's own number");
    } else if !from.is_empty() {
        let message = Inbound::text(field("MessageSid"), from, "dm", from, field("Body"));
        if let Ok(mut inbox) = INBOX.lock() {
            inbox.push(message);
        }
    }
    respond(200, "text/xml", "<Response/>")
}

struct Sms;

impl Guest for Sms {
    fn descriptor() -> String {
        json!({
            "platform": "sms",
            "max_message_length": 1600,
            "length_unit": "chars",
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
        let public_url = required(&options, "public_url")?;
        let settings = Settings {
            api: options["api_base"]
                .as_str()
                .unwrap_or("https://api.twilio.com")
                .trim_end_matches('/')
                .to_owned(),
            account_sid: required(&options, "account_sid")?,
            phone_number: required(&options, "phone_number")?,
            public_url,
        };
        if host::secret(AUTH_TOKEN).is_none() {
            return Err(format!("secret {AUTH_TOKEN} is not set"));
        }
        *SETTINGS.lock().map_err(|e| e.to_string())? = Some(settings);
        Ok(())
    }

    fn stop() {}

    fn poll() -> Result<String, String> {
        let drained: Vec<Inbound> = INBOX
            .lock()
            .map(|mut i| i.drain(..).collect())
            .unwrap_or_default();
        Ok(message::batch(&drained))
    }

    fn send(message: String) -> Result<String, String> {
        let out = Outgoing::parse(&message)?;
        let token = host::secret(AUTH_TOKEN)
            .ok_or_else(|| format!("permanent: secret {AUTH_TOKEN} is not set"))?;
        let (url, sid, from) = with_settings(|s| {
            (
                format!(
                    "{}/2010-04-01/Accounts/{}/Messages.json",
                    s.api, s.account_sid
                ),
                s.account_sid.clone(),
                s.phone_number.clone(),
            )
        })?;
        let body = text::form_encode(&[
            ("To", &out.chat),
            ("From", &from),
            ("Body", &text::strip_markdown(&out.text)),
        ]);
        let auth = format!(
            "Basic {}",
            sign::base64(format!("{sid}:{token}").as_bytes())
        );
        let response = net::send(
            "POST",
            &url,
            &[
                ("authorization", &auth),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            Some(body.into_bytes()),
        )?;
        if let Err(why) = net::classify(&response) {
            host::warn(&format!(
                "Twilio refused an SMS to {}: {why}",
                redact(&out.chat)
            ));
            return Err(why);
        }
        Ok(response.json()["sid"]
            .as_str()
            .unwrap_or_default()
            .to_owned())
    }

    fn chat_info(chat: String) -> Result<String, String> {
        Ok(message::chat_info(&redact(&chat), "dm"))
    }

    fn handle_http(request: HttpRequest) -> HttpResponse {
        if request.method != "POST" {
            return respond(405, "text/plain", "POST only");
        }
        receive(&request)
    }

    fn authorize_sender(_inbound: String) -> String {
        "unknown".to_owned()
    }
}

adapter_kit::bindings::export_adapter!(Sms);
