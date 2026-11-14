//! The loopback forwarder of a `remote` or `ssh` connection: the webview
//! dials `ws://127.0.0.1:<port>/rpc` like the local daemon, and each
//! accepted socket is carried to the connection's daemon by its
//! [`Upstream`], which checks and rewrites the upgrade request first (a
//! `remote` one swaps the shell's one-use ticket for the device
//! credential, which never reaches the webview). Nothing but an upgrade
//! request on the client protocol path is forwarded.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use super::tls::BoxStream;

/// The largest request head read.
const MAX_HEAD: usize = 16 << 10;
/// How long a client may take to send its request head.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// An upgrade request's head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    /// `GET`.
    pub method: String,
    /// The request target.
    pub path: String,
    /// The headers in order.
    pub headers: Vec<(String, String)>,
}

impl Head {
    /// Parses a head that ends with an empty line.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?;
        let mut lines = text.split("\r\n");
        let mut first = lines.next()?.split(' ');
        let (method, path) = (first.next()?.to_owned(), first.next()?.to_owned());
        let headers = lines
            .filter(|l| !l.is_empty())
            .map(|l| {
                let (name, value) = l.split_once(':')?;
                Some((name.trim().to_owned(), value.trim().to_owned()))
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            method,
            path,
            headers,
        })
    }

    /// The value of header `name`.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Replaces header `name` (`None` removes it).
    pub fn set(&mut self, name: &str, value: Option<String>) {
        self.headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
        if let Some(value) = value {
            self.headers.push((name.to_owned(), value));
        }
    }

    /// The offered subprotocols.
    #[must_use]
    pub fn protocols(&self) -> Vec<String> {
        self.headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case("sec-websocket-protocol"))
            .flat_map(|(_, v)| v.split(','))
            .map(|p| p.trim().to_owned())
            .filter(|p| !p.is_empty())
            .collect()
    }

    /// The head as bytes, ending with the empty line.
    #[must_use]
    pub fn render(&self) -> Vec<u8> {
        let mut out = format!("{} {} HTTP/1.1\r\n", self.method, self.path);
        for (name, value) in &self.headers {
            out.push_str(name);
            out.push_str(": ");
            out.push_str(value);
            out.push_str("\r\n");
        }
        out.push_str("\r\n");
        out.into_bytes()
    }
}

/// What an upstream answers for one accepted socket.
pub type Opened<'a> = Pin<Box<dyn Future<Output = Result<(Head, BoxStream), String>> + Send + 'a>>;

/// Where a forwarder's sockets go.
pub trait Upstream: Send + Sync {
    /// Checks and rewrites `head` and opens the stream it goes to.
    fn open(&self, head: Head) -> Opened<'_>;
}

/// A running forwarder; it stops when dropped.
#[derive(Debug)]
pub struct Forwarder {
    /// The loopback port the webview dials.
    pub port: u16,
    task: JoinHandle<()>,
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Forwarder {
    /// Binds a loopback port and forwards every socket through `upstream`.
    ///
    /// # Errors
    ///
    /// The port could not be bound.
    pub async fn start(upstream: Arc<dyn Upstream>) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let task = pmt_core::spawn::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let upstream = Arc::clone(&upstream);
                pmt_core::spawn::spawn(async move { forward(socket, upstream.as_ref()).await });
            }
        });
        Ok(Self { port, task })
    }
}

async fn forward(mut client: TcpStream, upstream: &dyn Upstream) {
    let Ok(Some((head, rest))) = tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut client)).await
    else {
        return;
    };
    let is_upgrade = head
        .header("upgrade")
        .is_some_and(|u| u.eq_ignore_ascii_case("websocket"));
    if head.method != "GET" || !is_upgrade {
        let _ = refuse(&mut client, 404, "only the client protocol is forwarded").await;
        return;
    }
    match upstream.open(head).await {
        Ok((head, mut server)) => {
            if server.write_all(&head.render()).await.is_err()
                || server.write_all(&rest).await.is_err()
            {
                return;
            }
            let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
        }
        Err(why) => {
            tracing::warn!(why, "a forwarded dial was refused");
            let _ = refuse(&mut client, 403, &why).await;
        }
    }
}

/// The head, and what the client sent after it.
async fn read_head(client: &mut TcpStream) -> Option<(Head, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 2048];
    loop {
        let n = client.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = Head::parse(&buf[..end + 2])?;
            return Some((head, buf[end + 4..].to_vec()));
        }
        if buf.len() > MAX_HEAD {
            return None;
        }
    }
}

async fn refuse(client: &mut TcpStream, status: u16, why: &str) -> std::io::Result<()> {
    let body = why.as_bytes();
    let head = format!(
        "HTTP/1.1 {status} Refused\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    client.write_all(head.as_bytes()).await?;
    client.write_all(body).await?;
    client.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::Head;

    #[test]
    fn a_head_round_trips_and_headers_are_replaced() {
        let raw = b"GET /rpc HTTP/1.1\r\nHost: 127.0.0.1:5\r\nSec-WebSocket-Protocol: prometheus-rpc.v1, prometheus-ticket.abc\r\nOrigin: tauri://localhost\r\n";
        let mut head = Head::parse(raw).unwrap();
        assert_eq!(head.path, "/rpc");
        assert_eq!(
            head.protocols(),
            ["prometheus-rpc.v1", "prometheus-ticket.abc"]
        );
        head.set("origin", None);
        head.set("Host", Some("box.example".into()));
        let text = String::from_utf8(head.render()).unwrap();
        assert!(text.starts_with("GET /rpc HTTP/1.1\r\n"), "{text}");
        assert!(text.contains("Host: box.example\r\n"), "{text}");
        assert!(!text.to_ascii_lowercase().contains("origin"), "{text}");
        assert!(text.ends_with("\r\n\r\n"), "{text}");
    }
}
