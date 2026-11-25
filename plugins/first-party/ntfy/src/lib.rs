//! The ntfy adapter (spec: platform-adapters, ntfy adapter): a JSON
//! stream subscription to `options.topic` on `options.server_url`
//! (default `https://ntfy.sh`) held by the socket bridge, and replies
//! published to `options.publish_topic` (default the subscribe topic),
//! optionally as Markdown, truncated past 4096 characters with a notice.
//!
//! The topic is the only identity: every message on it comes from the
//! "user" named by the topic, and a publisher's title is shown, never
//! trusted. With no allowlist (`allow_users`, `allow_chats` or
//! `allow_all` of the platform table, handed in as `options.access`) the
//! adapter is outgoing-only: it never subscribes, so it admits nothing,
//! and still publishes cron deliveries and `gateway send`.
//!
//! A stream with no line for 55 seconds is dead (ntfy sends keep-alives
//! more often); reconnects wait 2, 5, 10, 30, then 60 seconds, starting
//! over once a connection stayed up 60 seconds, and resume after the last
//! message seen. 401 and 404 stop the adapter (`ntfy_unauthorized`,
//! `ntfy_topic_not_found`).

use std::sync::Mutex;

use adapter_kit::bridge::Bridge;
use adapter_kit::message::{self, Inbound, Outgoing};
use adapter_kit::{Guest, HttpRequest, HttpResponse, clock, host, net, respond, text};
use serde_json::{Value, json};

const MAX_CHARS: usize = 4096;
const TRUNCATED: &str = "\n\n[truncated: the full reply was longer than ntfy allows]";
const TOKEN: &str = "NTFY_TOKEN";
/// A stream silent this long is dead.
const KEEPALIVE_SECS: u64 = 55;
/// Reconnect waits, in order; the last repeats.
const BACKOFF_SECS: [u64; 5] = [2, 5, 10, 30, 60];
/// A connection up this long resets the backoff.
const STABLE_SECS: u64 = 60;

struct Ntfy {
    server: String,
    topic: String,
    publish_topic: String,
    markdown: bool,
    /// `None` when outgoing-only.
    stream: Option<Stream>,
}

struct Stream {
    bridge: Bridge,
    connection: Option<String>,
    connected_ms: u64,
    last_line_ms: u64,
    failures: usize,
    retry_at_ms: u64,
}

static NTFY: Mutex<Option<Ntfy>> = Mutex::new(None);

fn auth() -> Option<String> {
    host::secret(TOKEN).map(|t| format!("Bearer {t}"))
}

/// Whether the platform table admits anyone (`options.access`).
fn has_allowlist(options: &Value) -> bool {
    let access = &options["access"];
    access["allow_all"].as_bool().unwrap_or(false)
        || ["allow_users", "allow_chats"]
            .iter()
            .any(|k| access[k].as_array().is_some_and(|a| !a.is_empty()))
}

/// What a failed stream means: a fatal code for 401/404, else `None`.
fn fatal(error: &str) -> Option<String> {
    if error.contains("status 401") || error.contains("status 403") {
        Some(format!(
            "fatal: ntfy_unauthorized: the server refused the subscription ({error})"
        ))
    } else if error.contains("status 404") {
        Some(format!(
            "fatal: ntfy_topic_not_found: no such topic ({error})"
        ))
    } else {
        None
    }
}

/// The reconnect wait after `failures` consecutive failures.
fn backoff_secs(failures: usize) -> u64 {
    BACKOFF_SECS[failures.min(BACKOFF_SECS.len()) - 1]
}

/// `text` within the limit, with a notice when cut.
fn truncate(text: &str) -> String {
    if text.chars().count() <= MAX_CHARS {
        return text.to_owned();
    }
    let keep = MAX_CHARS - TRUNCATED.chars().count();
    let mut out: String = text.chars().take(keep).collect();
    out.push_str(TRUNCATED);
    out
}

impl Ntfy {
    fn subscribe_url(&self) -> String {
        let mut url = format!("{}/{}/json", self.server, self.topic);
        if let Some(since) = host::load("since").and_then(|v| v.as_str().map(str::to_owned)) {
            url.push_str("?since=");
            url.push_str(&text::percent_encode(&since));
        }
        url
    }

    /// One event line: a message, or `None` (open, keepalive, other
    /// topics).
    fn event(&self, line: &str) -> Option<Inbound> {
        let event: Value = serde_json::from_str(line).ok()?;
        if event["event"] != "message" || event["topic"] != self.topic.as_str() {
            return None;
        }
        let id = event["id"].as_str()?;
        host::store("since", &json!(id));
        let body = event["message"].as_str().unwrap_or_default();
        // A publisher's title is content, never identity.
        let text = match event["title"].as_str() {
            Some(title) if !title.is_empty() => format!("{title}\n{body}"),
            _ => body.to_owned(),
        };
        Some(Inbound::text(id, &self.topic, "dm", &self.topic, &text))
    }
}

impl Stream {
    fn connect(&mut self, url: &str) -> Result<(), String> {
        let auth = auth();
        let headers: Vec<(&str, &str)> = auth
            .as_deref()
            .map(|a| vec![("authorization", a)])
            .unwrap_or_default();
        let id = self.bridge.open(url, &headers)?;
        self.connection = Some(id);
        self.connected_ms = clock::now_ms();
        self.last_line_ms = self.connected_ms;
        Ok(())
    }

    /// Drops the connection and schedules the next attempt.
    fn lost(&mut self) {
        if let Some(id) = self.connection.take() {
            self.bridge.close(&id);
        }
        if clock::elapsed(self.connected_ms, STABLE_SECS) {
            self.failures = 0;
        }
        self.failures += 1;
        self.retry_at_ms = clock::now_ms() + backoff_secs(self.failures) * 1000;
    }
}

fn poll_stream(ntfy: &mut Ntfy) -> Result<Vec<Inbound>, String> {
    let url = ntfy.subscribe_url();
    let Some(stream) = ntfy.stream.as_mut() else {
        return Ok(Vec::new());
    };
    let Some(id) = stream.connection.clone() else {
        if clock::now_ms() >= stream.retry_at_ms && stream.connect(&url).is_err() {
            stream.lost();
        }
        return Ok(Vec::new());
    };
    let received = stream.bridge.recv(&id)?;
    if !received.frames.is_empty() {
        stream.last_line_ms = clock::now_ms();
    }
    let inbound: Vec<Inbound> = received
        .frames
        .iter()
        .filter_map(|line| ntfy.event(line))
        .collect();
    let stream = ntfy.stream.as_mut().ok_or("not started")?;
    if !received.open {
        let error = received.error.unwrap_or_default();
        if let Some(fatal) = fatal(&error) {
            return Err(fatal);
        }
        stream.lost();
    } else if clock::elapsed(stream.last_line_ms, KEEPALIVE_SECS) {
        host::info("the ntfy stream was silent past its keep-alive; reconnecting");
        stream.lost();
    }
    Ok(inbound)
}

struct Adapter;

impl Guest for Adapter {
    fn descriptor() -> String {
        json!({
            "platform": "ntfy",
            "max_message_length": MAX_CHARS,
            "length_unit": "chars",
            "splits_long_messages": true,
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
            "binds_http": false,
        })
        .to_string()
    }

    fn start(options: String) -> Result<(), String> {
        let options: Value = serde_json::from_str(&options).map_err(|e| e.to_string())?;
        let topic = options["topic"]
            .as_str()
            .filter(|t| !t.is_empty())
            .ok_or("options.topic is required")?
            .to_owned();
        let server = options["server_url"]
            .as_str()
            .unwrap_or("https://ntfy.sh")
            .trim_end_matches('/')
            .to_owned();
        let stream = if has_allowlist(&options) {
            Some(Stream {
                bridge: Bridge::from_options(&options)?,
                connection: None,
                connected_ms: 0,
                last_line_ms: 0,
                failures: 0,
                retry_at_ms: 0,
            })
        } else {
            host::info("ntfy has no allowlist: outgoing-only, the topic is not subscribed");
            None
        };
        let mut ntfy = Ntfy {
            publish_topic: options["publish_topic"]
                .as_str()
                .filter(|t| !t.is_empty())
                .unwrap_or(&topic)
                .to_owned(),
            server,
            topic,
            markdown: options["markdown"].as_bool().unwrap_or(false),
            stream,
        };
        let url = ntfy.subscribe_url();
        if let Some(stream) = ntfy.stream.as_mut() {
            stream.connect(&url)?;
        }
        let mut slot = NTFY.lock().map_err(|e| e.to_string())?;
        if let Some(old) = slot.take()
            && let Some(mut stream) = old.stream
        {
            stream.lost();
        }
        *slot = Some(ntfy);
        Ok(())
    }

    fn stop() {
        if let Ok(mut slot) = NTFY.lock()
            && let Some(ntfy) = slot.take()
            && let Some(mut stream) = ntfy.stream
        {
            stream.lost();
        }
    }

    fn poll() -> Result<String, String> {
        clock::tick();
        let mut slot = NTFY.lock().map_err(|e| e.to_string())?;
        let ntfy = slot.as_mut().ok_or("not started")?;
        poll_stream(ntfy).map(|inbound| message::batch(&inbound))
    }

    fn send(message: String) -> Result<String, String> {
        let out = Outgoing::parse(&message)?;
        let (url, markdown) = {
            let slot = NTFY.lock().map_err(|e| e.to_string())?;
            let ntfy = slot.as_ref().ok_or("not started")?;
            (
                format!("{}/{}", ntfy.server, ntfy.publish_topic),
                ntfy.markdown,
            )
        };
        let body = if markdown {
            out.text.clone()
        } else {
            text::strip_markdown(&out.text)
        };
        let auth = auth();
        let mut headers = vec![("content-type", "text/plain; charset=utf-8")];
        if markdown {
            headers.push(("markdown", "yes"));
        }
        if let Some(auth) = auth.as_deref() {
            headers.push(("authorization", auth));
        }
        let response = net::send("POST", &url, &headers, Some(truncate(&body).into_bytes()))?;
        net::classify(&response)?;
        Ok(response.json()["id"]
            .as_str()
            .unwrap_or_default()
            .to_owned())
    }

    fn chat_info(chat: String) -> Result<String, String> {
        Ok(message::chat_info(&chat, "dm"))
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
    fn the_backoff_climbs_to_a_minute() {
        let waits: Vec<u64> = (1..=7).map(backoff_secs).collect();
        assert_eq!(waits, [2, 5, 10, 30, 60, 60, 60]);
    }

    #[test]
    fn long_bodies_are_cut_with_a_notice() {
        let cut = truncate(&"x".repeat(5000));
        assert_eq!(cut.chars().count(), MAX_CHARS);
        assert!(cut.ends_with(TRUNCATED));
        assert_eq!(truncate("short"), "short");
    }

    #[test]
    fn refusals_are_fatal_with_their_codes() {
        assert!(
            fatal("status 401")
                .unwrap()
                .starts_with("fatal: ntfy_unauthorized:")
        );
        assert!(
            fatal("status 404")
                .unwrap()
                .starts_with("fatal: ntfy_topic_not_found:")
        );
        assert_eq!(fatal("socket hang up"), None);
    }

    #[test]
    fn no_allowlist_means_outgoing_only() {
        assert!(!has_allowlist(&json!({})));
        assert!(!has_allowlist(&json!({"access": {"allow_users": []}})));
        assert!(has_allowlist(
            &json!({"access": {"allow_users": ["alerts"]}})
        ));
        assert!(has_allowlist(&json!({"access": {"allow_all": true}})));
    }
}
