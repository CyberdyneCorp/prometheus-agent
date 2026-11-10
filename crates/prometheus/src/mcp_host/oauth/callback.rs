//! Receiving the authorization response of a browser login: on the
//! loopback redirect, or pasted by the person (spec: plugin-system, MCP
//! OAuth for HTTP servers).
//!
//! The listener binds `127.0.0.1` only and answers every request until
//! one arrives on the callback path carrying the login's `state`; the
//! person may instead paste the URL the browser was redirected to (a
//! remote machine, a blocked port). Either way the query parameters come
//! back for the caller to check.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use reqwest::Url;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

/// Longest request head read from one connection.
const MAX_HEAD: usize = 16 * 1024;

/// The bound loopback listener.
pub(super) struct Callback {
    listener: TcpListener,
    path: String,
}

impl Callback {
    /// Binds `127.0.0.1:<port>` (0: any free port) for callbacks on `path`.
    ///
    /// # Errors
    ///
    /// The port could not be bound.
    pub(super) async fn bind(port: u16, path: &str) -> Result<Self, String> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
            .await
            .map_err(|e| {
                format!("cannot listen on 127.0.0.1:{port} for the login callback: {e}")
            })?;
        Ok(Self {
            listener,
            path: path.to_owned(),
        })
    }

    /// The bound port.
    #[must_use]
    pub(super) fn port(&self) -> u16 {
        self.listener.local_addr().map_or(0, |a| a.port())
    }

    /// The query of the first callback carrying `state`.
    pub(super) async fn wait(&self, state: &str) -> HashMap<String, String> {
        loop {
            let Ok((mut stream, _)) = self.listener.accept().await else {
                continue;
            };
            let query = read_target(&mut stream)
                .await
                .and_then(|target| parse(&format!("http://127.0.0.1{target}"), &self.path))
                .filter(|q| q.get("state").map(String::as_str) == Some(state));
            let (status, body) = if query.is_some() {
                ("200 OK", "Signed in. You can close this window.")
            } else {
                ("404 Not Found", "Not the login callback.")
            };
            let reply = format!(
                "HTTP/1.1 {status}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes()).await;
            if let Some(query) = query {
                return query;
            }
        }
    }
}

/// The request target of one request head (`None` when it does not come
/// within 5 seconds).
async fn read_target(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut buf = [0u8; 2048];
    let read = tokio::time::timeout(Duration::from_secs(5), async {
        while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < MAX_HEAD {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => head.extend_from_slice(&buf[..n]),
            }
        }
    })
    .await;
    read.ok()?;
    String::from_utf8_lossy(&head)
        .split_whitespace()
        .nth(1)
        .map(str::to_owned)
}

/// The query parameters of a redirect `url` on `path`.
#[must_use]
pub(super) fn parse(url: &str, path: &str) -> Option<HashMap<String, String>> {
    let url = Url::parse(url.trim()).ok()?;
    (url.path() == path).then(|| url.query_pairs().into_owned().collect())
}

/// Reads one pasted redirect URL from stdin (a blocking thread, so the
/// listener keeps answering meanwhile); `None` at end of input.
pub(super) async fn pasted() -> Option<String> {
    pmt_core::spawn::blocking(|| {
        let mut line = String::new();
        loop {
            line.clear();
            match std::io::stdin().read_line(&mut line) {
                Ok(0) | Err(_) => return None,
                Ok(_) if line.trim().starts_with("http") => return Some(line.trim().to_owned()),
                Ok(_) => {}
            }
        }
    })
    .await
    .ok()
    .flatten()
}
