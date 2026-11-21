//! The IRC adapter (spec: platform-adapters, IRC adapter): one connection
//! to `options.server` on `options.port` (6697 with `use_tls`, the
//! default, else 6667) held by the socket bridge, registered as
//! `options.nickname` (default `prometheus-bot`) with the optional server
//! password and NickServ `IDENTIFY` from secrets. Joined channels are
//! groups (a message naming the bot is a mention), private messages are
//! DMs; replies are plain `PRIVMSG` lines within 450 bytes. No media,
//! threads, reactions or typing. The descriptor's `credential_options`
//! make the gateway lock the server and nickname across homes.

use std::sync::Mutex;

use adapter_kit::bridge::Bridge;
use adapter_kit::message::{self, Inbound, Outgoing};
use adapter_kit::{Guest, HttpRequest, HttpResponse, host, respond, text};
use serde_json::{Value, json};

/// The longest message text of one line.
const MAX_LINE_BYTES: usize = 450;

struct Irc {
    bridge: Bridge,
    connection: String,
    nickname: String,
    channels: Vec<String>,
    registered: bool,
    /// The last message number handed out (kept in profile state, so ids
    /// stay unique across restarts).
    seq: u64,
}

static IRC: Mutex<Option<Irc>> = Mutex::new(None);

/// One parsed protocol line.
#[derive(Debug, PartialEq, Eq)]
struct Line<'a> {
    nick: &'a str,
    command: &'a str,
    params: Vec<&'a str>,
}

fn parse(line: &str) -> Option<Line<'_>> {
    // IRCv3 tags are not requested; skip them if a server sends some.
    let line = match line.strip_prefix('@') {
        Some(rest) => rest.split_once(' ')?.1,
        None => line,
    };
    let (nick, rest) = match line.strip_prefix(':') {
        Some(rest) => {
            let (prefix, rest) = rest.split_once(' ')?;
            (prefix.split('!').next().unwrap_or(prefix), rest)
        }
        None => ("", line),
    };
    let (head, trailing) = match rest.split_once(" :") {
        Some((head, trailing)) => (head, Some(trailing)),
        None => (rest, None),
    };
    let mut words = head.split(' ').filter(|w| !w.is_empty());
    let command = words.next()?;
    let mut params: Vec<&str> = words.collect();
    params.extend(trailing);
    Some(Line {
        nick,
        command,
        params,
    })
}

fn is_channel(target: &str) -> bool {
    target.starts_with(['#', '&', '+', '!'])
}

/// The text without a leading `nick:`/`nick,` address, and whether it
/// named the bot.
fn addressed(text: &str, nickname: &str) -> (String, bool) {
    let lower = text.to_lowercase();
    let nick = nickname.to_lowercase();
    for sep in [':', ','] {
        if let Some(rest) = lower.strip_prefix(&format!("{nick}{sep}")) {
            let cut = text.len() - rest.len();
            return (text[cut..].trim_start().to_owned(), true);
        }
    }
    let named = lower
        .split(|c: char| !(c.is_alphanumeric() || "-_[]\\`^{}|".contains(c)))
        .any(|w| w == nick);
    (text.to_owned(), named)
}

impl Irc {
    fn write(&self, line: &str) -> Result<(), String> {
        self.bridge.send(&self.connection, &format!("{line}\r\n"))
    }

    fn next_id(&mut self) -> String {
        self.seq += 1;
        host::store("seq", &json!(self.seq));
        self.seq.to_string()
    }

    fn welcomed(&mut self) -> Result<(), String> {
        self.registered = true;
        if let Some(password) = host::secret("IRC_NICKSERV_PASSWORD") {
            self.write(&format!("PRIVMSG NickServ :IDENTIFY {password}"))?;
        }
        for channel in &self.channels {
            self.write(&format!("JOIN {channel}"))?;
        }
        Ok(())
    }

    fn privmsg(&mut self, line: &Line<'_>) -> Option<Inbound> {
        let (target, body) = (*line.params.first()?, *line.params.get(1)?);
        // CTCP (actions, version queries) is not conversation.
        if body.starts_with('\u{1}') || line.nick.eq_ignore_ascii_case(&self.nickname) {
            return None;
        }
        let id = self.next_id();
        if is_channel(target) {
            let (body, mentioned) = addressed(body, &self.nickname);
            let mut message = Inbound::text(&id, target, "group", line.nick, &body);
            message.mentioned = mentioned;
            Some(message)
        } else {
            Some(Inbound::text(&id, line.nick, "dm", line.nick, body))
        }
    }

    fn handle(&mut self, raw: &str, inbound: &mut Vec<Inbound>) -> Result<(), String> {
        let Some(line) = parse(raw) else {
            return Ok(());
        };
        match line.command {
            "PING" => self.write(&format!("PONG :{}", line.params.first().unwrap_or(&""))),
            "001" => self.welcomed(),
            // The nickname is taken: try it with an underscore.
            "433" if !self.registered => {
                self.nickname.push('_');
                self.write(&format!("NICK {}", self.nickname))
            }
            "PRIVMSG" => {
                inbound.extend(self.privmsg(&line));
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

fn options_of(text: &str) -> Result<Value, String> {
    serde_json::from_str(text).map_err(|e| e.to_string())
}

struct Adapter;

impl Guest for Adapter {
    fn descriptor() -> String {
        json!({
            "platform": "irc",
            "max_message_length": MAX_LINE_BYTES,
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
            "binds_http": false,
            "credential_options": ["server", "nickname=prometheus-bot"],
        })
        .to_string()
    }

    fn start(options: String) -> Result<(), String> {
        let options = options_of(&options)?;
        let server = options["server"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("options.server is required")?;
        let tls = options["use_tls"].as_bool().unwrap_or(true);
        let port = options["port"].as_u64().unwrap_or(if tls { 6697 } else { 6667 });
        let nickname = options["nickname"].as_str().unwrap_or("prometheus-bot").to_owned();
        let bridge = Bridge::from_options(&options)?;
        let mut slot = IRC.lock().map_err(|e| e.to_string())?;
        if let Some(old) = slot.take() {
            old.bridge.close(&old.connection);
        }
        let scheme = if tls { "tls" } else { "tcp" };
        let connection = bridge.open(&format!("{scheme}://{server}:{port}"), &[])?;
        let irc = Irc {
            bridge,
            connection,
            nickname: nickname.clone(),
            channels: adapter_kit::policy::strings(&options, "channels"),
            registered: false,
            seq: host::load("seq").and_then(|v| v.as_u64()).unwrap_or(0),
        };
        if let Some(password) = host::secret("IRC_SERVER_PASSWORD") {
            irc.write(&format!("PASS {password}"))?;
        }
        irc.write(&format!("NICK {nickname}"))?;
        irc.write(&format!("USER {nickname} 0 * :Prometheus"))?;
        *slot = Some(irc);
        Ok(())
    }

    fn stop() {
        if let Ok(mut slot) = IRC.lock()
            && let Some(irc) = slot.take()
        {
            let _ = irc.write("QUIT :bye");
            irc.bridge.close(&irc.connection);
        }
    }

    fn poll() -> Result<String, String> {
        adapter_kit::clock::tick();
        let mut slot = IRC.lock().map_err(|e| e.to_string())?;
        let irc = slot.as_mut().ok_or("not started")?;
        let received = irc.bridge.recv(&irc.connection)?;
        let mut inbound = Vec::new();
        for line in &received.frames {
            irc.handle(line, &mut inbound)?;
        }
        if !received.open && inbound.is_empty() {
            return Err(format!(
                "the IRC connection closed: {}",
                received.error.unwrap_or_else(|| "by the server".to_owned())
            ));
        }
        Ok(message::batch(&inbound))
    }

    fn send(message: String) -> Result<String, String> {
        let out = Outgoing::parse(&message)?;
        let mut slot = IRC.lock().map_err(|e| e.to_string())?;
        let irc = slot.as_mut().ok_or("not started")?;
        if !irc.registered {
            return Err("not registered with the IRC server yet".to_owned());
        }
        let plain = text::strip_markdown(&out.text);
        for piece in text::split_bytes(&plain, MAX_LINE_BYTES) {
            irc.write(&format!("PRIVMSG {} :{piece}", out.chat))?;
        }
        Ok(irc.next_id())
    }

    fn chat_info(chat: String) -> Result<String, String> {
        let kind = if is_channel(&chat) { "group" } else { "dm" };
        Ok(message::chat_info(&chat, kind))
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
    fn lines_parse_with_prefix_and_trailing() {
        let line = parse(":alice!a@host PRIVMSG #rust :hello: there").unwrap();
        assert_eq!(line.nick, "alice");
        assert_eq!(line.command, "PRIVMSG");
        assert_eq!(line.params, vec!["#rust", "hello: there"]);
        assert_eq!(parse("PING :srv").unwrap().params, vec!["srv"]);
    }

    #[test]
    fn a_leading_nick_address_is_a_mention_and_is_stripped() {
        assert_eq!(addressed("pbot: status?", "PBot"), ("status?".to_owned(), true));
        assert_eq!(addressed("ask pbot later", "pbot").1, true);
        assert_eq!(addressed("pbotty", "pbot").1, false);
    }
}
