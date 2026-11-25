//! The Buzz adapter (spec: platform-adapters, Buzz Nostr adapter): Block's
//! Nostr-based community relay. Inbound rides a NIP-42-authenticated relay
//! WebSocket (held by the sidecar's socket bridge): the `AUTH` challenge
//! is answered with a kind-22242 event signed here, then each channel in
//! `options.channels` (and DMs: chat events p-tagged to the bot) is
//! subscribed from a per-channel high-water mark seeded at connect, so no
//! history is replayed. With `transport = "auto"` (default) a relay that
//! cannot be reached or authenticated falls back to polling `buzz
//! messages get` every `poll_interval_secs` (4); `websocket` and `poll`
//! force one transport. A socket idle for 5 minutes is reopened.
//!
//! Everything sent goes through the Buzz CLI (`options.cli_path`, default
//! `buzz`), which the sidecar runs with the private key in its
//! environment, never on its command line. Replies thread by NIP-10
//! (`--reply-to` the thread root) unless `reply_in_thread = false`.
//!
//! Channels answer only when the bot is mentioned (by `options.name`, its
//! npub or hex key, or a p-tag); DMs always. The bot's own events are
//! dropped by public key. Inbound media (`imeta`) is downloaded only for
//! an authorized sender, only over HTTPS from the relay's origin or
//! `options.attachment_hosts`, within its declared size, and kept only
//! when its SHA-256 matches the declared one; otherwise the agent is told
//! the attachment failed verification.

use std::sync::Mutex;

use adapter_kit::bridge::Bridge;
use adapter_kit::message::{self, Inbound, OutFile, Outgoing};
use adapter_kit::{Guest, HttpRequest, HttpResponse, clock, host, net, respond, sign};
use serde_json::{Value, json};

mod media;
mod nostr;

use nostr::Identity;

const SECRET: &str = "BUZZ_PRIVATE_KEY";
const MAX_CHARS: usize = 8000;
/// Chat messages, forum posts and forum comments.
const KINDS: [u64; 3] = [9, 45001, 45003];
const AUTH_SECS: u64 = 15;
const IDLE_SECS: u64 = 300;
const MAX_BACKOFF_SECS: u64 = 60;
const SEEN: usize = 2000;
const FETCH_LIMIT: u64 = 50;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Transport {
    Auto,
    Socket,
    Poll,
}

/// A watched conversation and its high-water mark (seconds).
struct Channel {
    id: String,
    dm: bool,
    since: u64,
}

struct Buzz {
    relay: String,
    cli: String,
    name: String,
    transport: Transport,
    poll_secs: u64,
    threads: bool,
    attachment_hosts: Vec<String>,
    access: Value,
    identity: Identity,
    bridge: Bridge,
    connection: Option<String>,
    authed: bool,
    auth_id: Option<String>,
    opened_ms: u64,
    last_frame_ms: u64,
    polling: bool,
    next_poll_ms: u64,
    retry_at_ms: u64,
    backoff_secs: u64,
    /// Wall-clock seconds at `clock_base_ms` (from the relay's `Date`).
    wall_base: u64,
    clock_base_ms: u64,
    channels: Vec<Channel>,
    seen: Vec<String>,
    /// Inbound event id -> its thread root.
    roots: Vec<(String, String)>,
}

static BZ: Mutex<Option<Buzz>> = Mutex::new(None);

/// The relay's HTTP origin (`wss://h` -> `https://h`).
fn origin_of(relay: &str) -> String {
    let (scheme, rest) = relay.split_once("://").unwrap_or(("wss", relay));
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let scheme = if scheme == "ws" { "http" } else { "https" };
    format!("{scheme}://{host}")
}

fn tag_values<'a>(event: &'a Value, name: &str) -> impl Iterator<Item = &'a str> {
    event["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(move |t| t[0] == name)
        .filter_map(|t| t[1].as_str())
}

/// The NIP-10 thread root of `event`, if it is a reply.
fn thread_root(event: &Value) -> Option<String> {
    let tags = event["tags"].as_array()?;
    let marked = |marker: &str| {
        tags.iter()
            .find(|t| t[0] == "e" && t[3] == marker)
            .and_then(|t| t[1].as_str())
            .map(str::to_owned)
    };
    marked("root").or_else(|| marked("reply"))
}

impl Buzz {
    fn now(&self) -> u64 {
        self.wall_base + (clock::now_ms() - self.clock_base_ms) / 1000
    }

    /// Reads the wall clock from the relay origin's `Date` header.
    fn sync_clock(&mut self) {
        let Ok(response) = net::send("GET", &format!("{}/", origin_of(&self.relay)), &[], None)
        else {
            return;
        };
        if let Some(secs) = response.header("date").and_then(media::parse_http_date) {
            self.wall_base = secs;
            self.clock_base_ms = clock::now_ms();
        }
    }

    fn lost(&mut self) {
        if let Some(id) = self.connection.take() {
            self.bridge.close(&id);
        }
        self.authed = false;
        self.auth_id = None;
        self.retry_at_ms = clock::now_ms() + self.backoff_secs * 1000;
        self.backoff_secs = (self.backoff_secs * 2).min(MAX_BACKOFF_SECS);
    }

    fn open(&mut self) {
        self.sync_clock();
        match self.bridge.open(&self.relay, &[]) {
            Ok(id) => {
                self.connection = Some(id);
                self.opened_ms = clock::now_ms();
                self.last_frame_ms = self.opened_ms;
            }
            Err(_) => self.unreachable(),
        }
    }

    /// The relay cannot be used now: poll instead (auto) or retry later.
    fn unreachable(&mut self) {
        self.lost();
        if self.transport == Transport::Auto && !self.polling {
            host::warn("the Buzz relay is unreachable; polling with the Buzz CLI instead");
            self.polling = true;
        }
    }

    fn write(&self, frame: &Value) {
        if let Some(id) = &self.connection {
            let _ = self.bridge.send(id, &frame.to_string());
        }
    }

    fn subscribe(&self) {
        for (n, channel) in self.channels.iter().enumerate() {
            let mut filter = json!({"kinds": KINDS, "since": channel.since});
            if channel.dm {
                filter["kinds"] = json!([9]);
                filter["#p"] = json!([self.identity.pubkey]);
            } else {
                filter["#h"] = json!([channel.id]);
            }
            self.write(&json!(["REQ", format!("c{n}"), filter]));
        }
    }

    fn handle(&mut self, frame: &str, inbound: &mut Vec<Inbound>) {
        let Ok(message) = serde_json::from_str::<Value>(frame) else {
            return;
        };
        match message[0].as_str() {
            Some("AUTH") => {
                let challenge = message[1].as_str().unwrap_or_default();
                match self.identity.auth(&self.relay, challenge, self.now()) {
                    Ok(event) => {
                        self.auth_id = event["id"].as_str().map(str::to_owned);
                        self.write(&json!(["AUTH", event]));
                    }
                    Err(why) => host::warn(&format!("Buzz AUTH not signed: {why}")),
                }
            }
            Some("OK") if message[1].as_str() == self.auth_id.as_deref() => {
                if message[2] == true {
                    self.authed = true;
                    self.polling = false;
                    self.backoff_secs = 2;
                    self.subscribe();
                } else {
                    host::warn(&format!("the Buzz relay refused AUTH: {}", message[3]));
                    self.unreachable();
                }
            }
            Some("EVENT") => inbound.extend(self.event(&message[2])),
            _ => {}
        }
    }

    fn mentioned(&self, event: &Value) -> bool {
        let content = event["content"].as_str().unwrap_or_default().to_lowercase();
        let named =
            !self.name.is_empty() && content.contains(&format!("@{}", self.name.to_lowercase()));
        named
            || content.contains(&self.identity.npub())
            || content.contains(&self.identity.pubkey)
            || tag_values(event, "p").any(|p| p.eq_ignore_ascii_case(&self.identity.pubkey))
    }

    /// Whether the platform table's allowlist admits `sender` (downloads
    /// wait for this; the gateway still decides the turn).
    fn authorized(&self, sender: &str) -> bool {
        self.access["allow_all"] == true
            || self.access["allow_users"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .any(|u| u.eq_ignore_ascii_case(sender))
    }

    /// The conversation an event belongs to.
    fn conversation(&self, event: &Value) -> Option<&Channel> {
        let h: Vec<&str> = tag_values(event, "h").collect();
        self.channels
            .iter()
            .find(|c| !c.dm && h.contains(&c.id.as_str()))
            .or_else(|| self.channels.iter().find(|c| c.dm))
    }

    fn event(&mut self, event: &Value) -> Option<Inbound> {
        let id = event["id"].as_str()?.to_owned();
        let author = event["pubkey"].as_str()?.to_lowercase();
        let kind = event["kind"].as_u64()?;
        if !KINDS.contains(&kind) || self.seen.contains(&id) {
            return None;
        }
        let created = event["created_at"].as_u64().unwrap_or(0);
        let channel = self.conversation(event)?;
        let (dm, chat) = (channel.dm, channel.id.clone());
        let chat = if dm {
            tag_values(event, "h").next().unwrap_or(&author).to_owned()
        } else {
            chat
        };
        self.seen.push(id.clone());
        if self.seen.len() > SEEN {
            self.seen.remove(0);
        }
        for c in self
            .channels
            .iter_mut()
            .filter(|c| c.dm == dm && (dm || c.id == chat))
        {
            c.since = c.since.max(created);
        }
        if author == self.identity.pubkey || (!dm && !self.mentioned(event)) {
            return None;
        }
        let root = thread_root(event);
        self.roots
            .push((id.clone(), root.clone().unwrap_or_else(|| id.clone())));
        if self.roots.len() > SEEN {
            self.roots.remove(0);
        }
        let mut text = event["content"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_owned();
        let mut files = Vec::new();
        if self.authorized(&author) {
            let (got, notes) =
                media::fetch_all(event, &origin_of(&self.relay), &self.attachment_hosts);
            files = got;
            for note in notes {
                text.push_str(&format!("\n{note}"));
            }
        }
        let mut message = Inbound::text(
            &id,
            &chat,
            if dm { "dm" } else { "group" },
            &author,
            text.trim(),
        );
        message.mentioned = !dm;
        message.thread = root;
        message.attachments = files;
        Some(message)
    }

    /// One CLI run through the sidecar: its parsed stdout.
    fn cli(&self, args: &[&str], input: &str, file: Option<&OutFile>) -> Result<Value, String> {
        let mut body =
            json!({"command": self.cli, "args": args, "input": input, "relay": self.relay});
        if let Some(file) = file {
            body["file"] = json!({"name": file.name, "data": sign::base64(&file.data)});
        }
        let answer = self.bridge.call("/cli", &body)?;
        if answer["code"] == 0 {
            let out = answer["stdout"].as_str().unwrap_or_default();
            return Ok(serde_json::from_str(out).unwrap_or_else(|_| json!(out)));
        }
        Err(classify(answer["stderr"].as_str().unwrap_or_default()))
    }

    fn poll_cli(&mut self) -> Vec<Inbound> {
        if clock::now_ms() < self.next_poll_ms {
            return Vec::new();
        }
        self.next_poll_ms = clock::now_ms() + self.poll_secs * 1000;
        let mut inbound = Vec::new();
        let targets: Vec<(String, u64)> = self
            .channels
            .iter()
            .filter(|c| !c.dm)
            .map(|c| (c.id.clone(), c.since))
            .collect();
        for (channel, since) in targets {
            let (limit, since) = (FETCH_LIMIT.to_string(), since.to_string());
            let args = [
                "messages",
                "get",
                "--channel",
                &channel,
                "--limit",
                &limit,
                "--since",
                &since,
            ];
            let Ok(events) = self.cli(&args, "", None) else {
                continue;
            };
            for event in events.as_array().into_iter().flatten() {
                inbound.extend(self.event(event));
            }
        }
        inbound
    }

    fn poll(&mut self) -> Result<Vec<Inbound>, String> {
        if self.polling || self.transport == Transport::Poll {
            let inbound = self.poll_cli();
            // Try the relay again now and then.
            if self.transport == Transport::Auto && clock::now_ms() >= self.retry_at_ms {
                self.open();
            } else {
                return Ok(inbound);
            }
        }
        let Some(id) = self.connection.clone() else {
            if clock::now_ms() >= self.retry_at_ms {
                self.open();
            }
            return Ok(Vec::new());
        };
        let received = self.bridge.recv(&id)?;
        let mut inbound = Vec::new();
        if !received.frames.is_empty() {
            self.last_frame_ms = clock::now_ms();
        }
        for frame in &received.frames {
            self.handle(frame, &mut inbound);
        }
        if !received.open || (!self.authed && clock::elapsed(self.opened_ms, AUTH_SECS)) {
            if self.authed {
                self.lost();
            } else {
                self.unreachable();
            }
        } else if clock::elapsed(self.last_frame_ms, IDLE_SECS) {
            self.lost();
            self.retry_at_ms = clock::now_ms();
        }
        Ok(inbound)
    }

    fn send(&mut self, out: &Outgoing) -> Result<String, String> {
        if !out.op.is_empty() {
            return Err("unsupported".to_owned());
        }
        let anchor = out.thread.clone().or_else(|| out.reply_to.clone());
        let root = anchor.map(|a| {
            self.roots
                .iter()
                .find(|(id, _)| *id == a)
                .map_or(a, |(_, root)| root.clone())
        });
        let mut args = vec![
            "messages",
            "send",
            "--channel",
            out.chat.as_str(),
            "--content",
            "-",
        ];
        if let Some(root) = root.as_deref().filter(|_| self.threads) {
            args.extend(["--reply-to", root]);
        }
        let mut last = None;
        for file in &out.attachments {
            let answer = self.cli(&args, "", Some(file))?;
            last = Some(event_id(&answer));
        }
        if out.text.trim().is_empty() {
            return last.ok_or_else(|| "permanent: nothing to send".to_owned());
        }
        let text: String = out.text.chars().take(MAX_CHARS).collect();
        self.cli(&args, &text, None).map(|a| event_id(&a))
    }
}

fn event_id(answer: &Value) -> String {
    answer["id"]
        .as_str()
        .or(answer["event_id"].as_str())
        .or(answer["event"]["id"].as_str())
        .unwrap_or("sent")
        .to_owned()
}

/// A CLI failure as the gateway retries it.
fn classify(stderr: &str) -> String {
    let lower = stderr.to_lowercase();
    if lower.contains("rate limit") || lower.contains("too many") {
        "rate_limited".to_owned()
    } else if [
        "timed out",
        "timeout",
        "unavailable",
        "connection",
        " 500",
        " 502",
        " 503",
    ]
    .iter()
    .any(|w| lower.contains(w))
    {
        format!("server: {}", stderr.trim())
    } else {
        format!("permanent: {}", stderr.trim())
    }
}

fn with<T>(f: impl FnOnce(&mut Buzz) -> Result<T, String>) -> Result<T, String> {
    let mut slot = BZ.lock().map_err(|e| e.to_string())?;
    f(slot.as_mut().ok_or("not started")?)
}

fn transport(options: &Value) -> Result<Transport, String> {
    match options["transport"].as_str().unwrap_or("auto") {
        "auto" => Ok(Transport::Auto),
        "websocket" => Ok(Transport::Socket),
        "poll" => Ok(Transport::Poll),
        other => Err(format!(
            "options.transport '{other}' is not auto, websocket or poll"
        )),
    }
}

fn buzz_of(options: &Value) -> Result<Buzz, String> {
    let relay = options["relay_url"]
        .as_str()
        .filter(|r| r.starts_with("wss://") || r.starts_with("ws://"))
        .ok_or("options.relay_url (wss://...) is required")?
        .to_owned();
    let key = host::secret(SECRET).ok_or_else(|| format!("secret {SECRET} is not set"))?;
    let identity = Identity::parse(&key)?;
    let mut channels: Vec<Channel> = adapter_kit::policy::strings(options, "channels")
        .into_iter()
        .map(|id| Channel {
            id,
            dm: false,
            since: 0,
        })
        .collect();
    channels.push(Channel {
        id: "dm".to_owned(),
        dm: true,
        since: 0,
    });
    Ok(Buzz {
        relay,
        cli: options["cli_path"].as_str().unwrap_or("buzz").to_owned(),
        name: options["name"].as_str().unwrap_or_default().to_owned(),
        transport: transport(options)?,
        poll_secs: options["poll_interval_secs"].as_u64().unwrap_or(4).max(1),
        threads: options["reply_in_thread"].as_bool().unwrap_or(true),
        attachment_hosts: adapter_kit::policy::strings(options, "attachment_hosts"),
        access: options["access"].clone(),
        identity,
        bridge: Bridge::from_options(options)?,
        connection: None,
        authed: false,
        auth_id: None,
        opened_ms: 0,
        last_frame_ms: 0,
        polling: false,
        next_poll_ms: 0,
        retry_at_ms: 0,
        backoff_secs: 2,
        wall_base: 0,
        clock_base_ms: 0,
        channels,
        seen: Vec::new(),
        roots: Vec::new(),
    })
}

struct Adapter;

impl Guest for Adapter {
    fn descriptor() -> String {
        json!({
            "platform": "buzz",
            "max_message_length": MAX_CHARS,
            "length_unit": "chars",
            "splits_long_messages": false,
            "supports_edit": false,
            "supports_delete": false,
            "supports_typing": false,
            "supports_reactions": false,
            "supports_threads": true,
            "interactive": "numbered",
            "media_in": ["image", "document"],
            "media_out": ["image", "document"],
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
        let mut bz = buzz_of(&options)?;
        bz.sync_clock();
        // The high-water marks start now: history is never replayed.
        let now = bz.now();
        for channel in &mut bz.channels {
            channel.since = now;
        }
        if bz.transport == Transport::Poll {
            bz.polling = true;
        } else {
            bz.open();
        }
        let mut slot = BZ.lock().map_err(|e| e.to_string())?;
        if let Some(mut old) = slot.take() {
            old.lost();
        }
        *slot = Some(bz);
        Ok(())
    }

    fn stop() {
        if let Ok(mut slot) = BZ.lock()
            && let Some(mut bz) = slot.take()
        {
            bz.lost();
        }
    }

    fn poll() -> Result<String, String> {
        clock::tick();
        with(Buzz::poll).map(|inbound| message::batch(&inbound))
    }

    fn send(message: String) -> Result<String, String> {
        let out = Outgoing::parse(&message)?;
        with(|bz| bz.send(&out))
    }

    fn chat_info(chat: String) -> Result<String, String> {
        // A watched channel is a group; any other conversation reached the
        // bot as a DM.
        let group = BZ.lock().ok().is_some_and(|slot| {
            slot.as_ref()
                .is_some_and(|bz| bz.channels.iter().any(|c| !c.dm && c.id == chat))
        });
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
    fn the_relay_origin_follows_its_scheme_and_failures_classify() {
        assert_eq!(
            origin_of("wss://relay.buzz.example/v1"),
            "https://relay.buzz.example"
        );
        assert_eq!(origin_of("ws://127.0.0.1:9000"), "http://127.0.0.1:9000");
        assert_eq!(classify("Error: rate limit exceeded"), "rate_limited");
        assert!(classify("relay unavailable (503)").starts_with("server:"));
        assert!(classify("not a member of this channel").starts_with("permanent:"));
    }

    #[test]
    fn a_reply_threads_under_its_root() {
        let event = json!({"tags": [["e", "R", "", "root"], ["e", "P", "", "reply"]]});
        assert_eq!(thread_root(&event).as_deref(), Some("R"));
        assert_eq!(
            thread_root(&json!({"tags": [["e", "P", "", "reply"]]})).as_deref(),
            Some("P")
        );
        assert_eq!(thread_root(&json!({"tags": []})), None);
    }
}
