//! A scripted test identity provider: a bare HTTP/1.1 server whose
//! handler answers each request with JSON or drops the connection without
//! answering (a transport error for the client), recording every request.

use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// One received request.
#[derive(Debug, Clone)]
pub(crate) struct Received {
    pub path: String,
    pub body: String,
}

impl Received {
    /// A form or JSON body field.
    pub(crate) fn field(&self, name: &str) -> Option<String> {
        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&self.body) {
            return map.get(name).and_then(Value::as_str).map(str::to_owned);
        }
        url::form_urlencoded::parse(self.body.as_bytes())
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    }
}

/// How to answer.
pub(crate) enum Action {
    Reply(u16, Value),
    /// Close the connection without answering.
    Reset,
}

type Handler = dyn Fn(&Received, usize) -> Action + Send + Sync;

/// The running server.
pub(crate) struct Idp {
    base: String,
    received: Arc<Mutex<Vec<Received>>>,
}

impl Idp {
    /// Starts a server; `handler` gets each request and how many earlier
    /// requests had the same path.
    pub(crate) async fn start(
        handler: impl Fn(&Received, usize) -> Action + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let received: Arc<Mutex<Vec<Received>>> = Arc::default();
        let handler: Arc<Handler> = Arc::new(handler);
        let log = Arc::clone(&received);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let handler = Arc::clone(&handler);
                let log = Arc::clone(&log);
                tokio::spawn(serve(stream, handler, log));
            }
        });
        Self { base, received }
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub(crate) fn received(&self, path: &str) -> Vec<Received> {
        let all = self.received.lock().unwrap();
        all.iter().filter(|r| r.path == path).cloned().collect()
    }
}

async fn serve(mut stream: TcpStream, handler: Arc<Handler>, log: Arc<Mutex<Vec<Received>>>) {
    let Some(request) = read(&mut stream).await else {
        return;
    };
    let seen = {
        let mut all = log.lock().unwrap();
        let seen = all.iter().filter(|r| r.path == request.path).count();
        all.push(request.clone());
        seen
    };
    match handler(&request, seen) {
        Action::Reset => drop(stream),
        Action::Reply(status, body) => {
            let body = body.to_string();
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes()).await;
            let _ = stream.write_all(body.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    }
}

async fn read(stream: &mut TcpStream) -> Option<Received> {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&buf[..n]);
    };
    let head = String::from_utf8_lossy(&data[..head_end]).to_string();
    let length: usize = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().ok())?
        })
        .unwrap_or(0);
    while data.len() < head_end + length {
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    let target = head.split(' ').nth(1)?.to_owned();
    Some(Received {
        path: target.split('?').next()?.to_owned(),
        body: String::from_utf8_lossy(&data[head_end..]).to_string(),
    })
}
