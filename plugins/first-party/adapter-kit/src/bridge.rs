//! The socket bridge: the sidecar (`bridge/bridge.mjs`) that holds the
//! long-lived connections the WASM sandbox cannot (WebSocket, TCP/TLS
//! line streams, HTTP line streams) for the adapters that need them. The
//! adapter reaches it through host HTTP on loopback with the per-start
//! token the host passes as `options.sidecar`; the bridge connects only to
//! the hosts the plugin was granted.
//!
//! - `POST /open {"url", "headers"?}`: `ws://`/`wss://` (WebSocket),
//!   `tcp://host:port`/`tls://host:port` (newline-framed), `http(s)://`
//!   (a streamed GET read line by line); answers `{"id"}`. With `"whole":
//!   true` (and `method`, `body`) an http(s) URL is one request (a long
//!   poll) whose answer arrives as a single frame `{"status", "body"}`.
//!   With `"binary": true` a WebSocket's binary frames cross as base64
//!   both ways.
//! - `POST /send {"id", "data"}`: a text frame (WebSocket) or raw text.
//! - `POST /recv {"id", "max"}`: `{"frames": [..], "open", "error"}`
//!   without waiting.
//! - `POST /close {"id"}`.

use serde_json::{Value, json};

use crate::net;

/// What `recv` answers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Received {
    /// Text frames (or lines), oldest first.
    pub frames: Vec<String>,
    /// Whether the connection is still open.
    pub open: bool,
    /// Why it closed, when it failed.
    pub error: Option<String>,
}

impl Received {
    fn parse(value: &Value) -> Self {
        Self {
            frames: value["frames"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|f| f.as_str().map(str::to_owned))
                .collect(),
            open: value["open"].as_bool().unwrap_or(false),
            error: value["error"].as_str().map(str::to_owned),
        }
    }
}

/// The bridge of one adapter start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bridge {
    base: String,
    token: String,
}

impl Bridge {
    /// The bridge the host started (`options.sidecar = {url, token}`).
    ///
    /// # Errors
    ///
    /// No sidecar in the options.
    pub fn from_options(options: &Value) -> Result<Self, String> {
        let url = options["sidecar"]["url"]
            .as_str()
            .ok_or("the socket bridge sidecar is not running (options.sidecar missing)")?;
        Ok(Self {
            base: url.trim_end_matches('/').to_owned(),
            token: options["sidecar"]["token"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        })
    }

    /// Posts `body` to the sidecar's `path`: a bridge route, or one a
    /// sidecar built on the bridge adds (Buzz's `/cli`).
    ///
    /// # Errors
    ///
    /// The sidecar refused or did not answer.
    pub fn call(&self, path: &str, body: &Value) -> Result<Value, String> {
        let auth = format!("Bearer {}", self.token);
        net::json(
            "POST",
            &format!("{}{path}", self.base),
            &[("authorization", &auth)],
            body,
        )
    }

    /// Opens a connection to `url` with `headers`; its id.
    ///
    /// # Errors
    ///
    /// The bridge refused (an ungranted host) or could not connect.
    pub fn open(&self, url: &str, headers: &[(&str, &str)]) -> Result<String, String> {
        let headers: serde_json::Map<String, Value> = headers
            .iter()
            .map(|(k, v)| ((*k).to_owned(), json!(v)))
            .collect();
        let answer = self.call("/open", &json!({"url": url, "headers": headers}))?;
        answer["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "the bridge answered no connection id".to_owned())
    }

    /// Opens a WebSocket whose binary frames cross as base64 (received
    /// frames are base64; [`Self::send`] takes base64).
    ///
    /// # Errors
    ///
    /// As [`Self::open`].
    pub fn open_binary(&self, url: &str) -> Result<String, String> {
        let answer = self.call("/open", &json!({"url": url, "binary": true}))?;
        answer["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "the bridge answered no connection id".to_owned())
    }

    /// Starts one HTTP request whose whole answer arrives later as a
    /// single frame `{"status", "body"}` (a long poll that must not block
    /// the adapter); the connection's id.
    ///
    /// # Errors
    ///
    /// The bridge refused (an ungranted host).
    pub fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Result<String, String> {
        let headers: serde_json::Map<String, Value> = headers
            .iter()
            .map(|(k, v)| ((*k).to_owned(), json!(v)))
            .collect();
        let answer = self.call(
            "/open",
            &json!({"url": url, "headers": headers, "method": method, "body": body, "whole": true}),
        )?;
        answer["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "the bridge answered no connection id".to_owned())
    }

    /// Sends one frame (or raw text).
    ///
    /// # Errors
    ///
    /// The connection is gone.
    pub fn send(&self, id: &str, data: &str) -> Result<(), String> {
        self.call("/send", &json!({"id": id, "data": data}))
            .map(|_| ())
    }

    /// What arrived since the last call.
    ///
    /// # Errors
    ///
    /// The bridge is unreachable or does not know `id`.
    pub fn recv(&self, id: &str) -> Result<Received, String> {
        self.call("/recv", &json!({"id": id, "max": 500}))
            .map(|v| Received::parse(&v))
    }

    /// Closes the connection (best effort).
    pub fn close(&self, id: &str) {
        let _ = self.call("/close", &json!({"id": id}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bridge_comes_from_the_sidecar_option() {
        let bridge =
            Bridge::from_options(&json!({"sidecar": {"url": "http://127.0.0.1:9/", "token": "t"}}))
                .unwrap();
        assert_eq!(bridge.base, "http://127.0.0.1:9");
        assert!(Bridge::from_options(&json!({})).is_err());
        let received =
            Received::parse(&json!({"frames": ["a", "b"], "open": false, "error": "reset"}));
        assert_eq!(received.frames, ["a", "b"]);
        assert_eq!(received.error.as_deref(), Some("reset"));
    }
}
