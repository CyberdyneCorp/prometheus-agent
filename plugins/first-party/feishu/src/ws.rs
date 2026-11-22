//! Feishu's long connection (the `websocket` connection mode): the app's
//! id and secret buy a WebSocket URL (`POST /callback/ws/endpoint`), the
//! socket bridge holds the socket in binary mode, and events arrive as
//! data frames ([`crate::frame`]) the adapter acknowledges one by one.
//! A ping goes out every `PingInterval` seconds (120 unless the server
//! says otherwise); a lost socket reconnects after 1 to 60 seconds.

use adapter_kit::bridge::Bridge;
use adapter_kit::{clock, net, sign};
use serde_json::{Value, json};

use crate::api::{Api, refusal};
use crate::frame::{CONTROL, DATA, Frame};

const MAX_BACKOFF_SECS: u64 = 60;
const DEFAULT_PING_SECS: u64 = 120;
/// Parts of split events kept at once.
const MAX_PARTIAL: usize = 32;

/// The connection.
pub struct Socket {
    bridge: Bridge,
    id: Option<String>,
    service: u64,
    ping_secs: u64,
    ping_ms: u64,
    backoff_secs: u64,
    retry_at_ms: u64,
    /// Events split into parts: (message id, parts by sequence).
    partial: Vec<(String, Vec<Option<Vec<u8>>>)>,
}

/// The `service_id` query parameter of the socket URL.
fn service_of(url: &str) -> u64 {
    adapter_kit::query(url)
        .into_iter()
        .find(|(k, _)| k == "service_id")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0)
}

impl Socket {
    /// A connection through `bridge`, not yet open.
    #[must_use]
    pub fn new(bridge: Bridge) -> Self {
        Self {
            bridge,
            id: None,
            service: 0,
            ping_secs: DEFAULT_PING_SECS,
            ping_ms: 0,
            backoff_secs: 1,
            retry_at_ms: 0,
            partial: Vec::new(),
        }
    }

    /// Opens the socket.
    ///
    /// # Errors
    ///
    /// `permanent:` when Feishu refuses the app's credentials; anything
    /// else is worth a retry.
    pub fn connect(&mut self, api: &Api) -> Result<(), String> {
        let body = json!({"AppID": api.app_id, "AppSecret": Api::secret()?});
        let response = net::send(
            "POST",
            &format!("{}/callback/ws/endpoint", api.base),
            &[("content-type", "application/json"), ("locale", "zh")],
            Some(body.to_string().into_bytes()),
        )?;
        let answer = response.json();
        if let Some(why) = refusal(response.status, &answer) {
            return Err(why);
        }
        let url = answer["data"]["URL"]
            .as_str()
            .ok_or("server: Feishu answered no long-connection URL")?;
        self.service = service_of(url);
        if let Some(secs) = answer["data"]["ClientConfig"]["PingInterval"].as_u64() {
            self.ping_secs = secs.max(5);
        }
        self.id = Some(self.bridge.open_binary(url)?);
        self.ping_ms = clock::now_ms();
        self.backoff_secs = 1;
        Ok(())
    }

    /// Closes the socket.
    pub fn close(&mut self) {
        if let Some(id) = self.id.take() {
            self.bridge.close(&id);
        }
    }

    fn lost(&mut self) {
        self.close();
        self.retry_at_ms = clock::now_ms() + self.backoff_secs * 1000;
        self.backoff_secs = (self.backoff_secs * 2).min(MAX_BACKOFF_SECS);
    }

    fn write(&self, frame: &Frame) {
        if let Some(id) = &self.id {
            let _ = self.bridge.send(id, &sign::base64(&frame.encode()));
        }
    }

    /// The events received since the last call (reconnecting when the
    /// socket is gone and its backoff has passed).
    ///
    /// # Errors
    ///
    /// `fatal:` when Feishu refuses the app's credentials.
    pub fn poll(&mut self, api: &Api) -> Result<Vec<Value>, String> {
        let Some(id) = self.id.clone() else {
            if clock::now_ms() >= self.retry_at_ms {
                match self.connect(api) {
                    Err(why) if why.starts_with("permanent:") => {
                        return Err(format!("fatal: feishu_auth_failed: {why}"));
                    }
                    Err(_) => self.lost(),
                    Ok(()) => {}
                }
            }
            return Ok(Vec::new());
        };
        let Ok(received) = self.bridge.recv(&id) else {
            self.lost();
            return Ok(Vec::new());
        };
        let mut events = Vec::new();
        for raw in &received.frames {
            let Some(frame) = sign::base64_decode(raw)
                .ok()
                .and_then(|b| Frame::decode(&b))
            else {
                continue;
            };
            events.extend(self.frame(&frame));
        }
        if !received.open {
            self.lost();
        } else if clock::elapsed(self.ping_ms, self.ping_secs) {
            self.ping_ms = clock::now_ms();
            self.write(&Frame::ping(self.service));
        }
        Ok(events)
    }

    /// One frame: a data frame's event once all its parts arrived.
    fn frame(&mut self, frame: &Frame) -> Option<Value> {
        match frame.method {
            CONTROL => {
                if frame.header("type") == "pong"
                    && let Ok(config) = serde_json::from_slice::<Value>(&frame.payload)
                    && let Some(secs) = config["PingInterval"].as_u64()
                {
                    self.ping_secs = secs.max(5);
                }
                None
            }
            DATA => {
                self.write(&frame.ack());
                let payload = self.assemble(frame)?;
                serde_json::from_slice(&payload).ok()
            }
            _ => None,
        }
    }

    /// The whole payload of a (possibly split) data frame, once complete.
    fn assemble(&mut self, frame: &Frame) -> Option<Vec<u8>> {
        let sum: usize = frame.header("sum").parse().unwrap_or(1);
        if sum <= 1 {
            return Some(frame.payload.clone());
        }
        let seq: usize = frame.header("seq").parse().ok()?;
        let id = frame.header("message_id").to_owned();
        let at = match self.partial.iter().position(|(m, _)| *m == id) {
            Some(at) => at,
            None => {
                if self.partial.len() >= MAX_PARTIAL {
                    self.partial.remove(0);
                }
                self.partial.push((id, vec![None; sum]));
                self.partial.len() - 1
            }
        };
        *self.partial[at].1.get_mut(seq)? = Some(frame.payload.clone());
        if self.partial[at].1.iter().any(Option::is_none) {
            return None;
        }
        let (_, parts) = self.partial.remove(at);
        Some(parts.into_iter().flatten().flatten().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_service_id_comes_from_the_socket_url() {
        assert_eq!(
            service_of("wss://msg-frontier.feishu.cn/ws/v2?device_id=7&service_id=33"),
            33
        );
        assert_eq!(service_of("wss://x/ws"), 0);
    }
}
