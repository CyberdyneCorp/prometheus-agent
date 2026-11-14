//! WebSocket helpers shared by the daemon's listeners' tests (the
//! dashboard's `/api/rpc` and the remote attachment listener).

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

/// Reads frames until the socket closes; its close code.
pub async fn close_code<S>(socket: &mut S) -> Option<u16>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let wait = std::time::Duration::from_secs(10);
    loop {
        match tokio::time::timeout(wait, socket.next()).await.ok()? {
            Some(Ok(Message::Close(frame))) => return frame.map(|f| u16::from(f.code)),
            Some(Ok(_)) => {}
            _ => return None,
        }
    }
}

/// Sends a request frame and returns its response (events in between are
/// skipped).
pub async fn request_on<S>(ws: &mut S, frame: &Value) -> Value
where
    S: futures_util::Sink<Message>
        + futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::fmt::Debug,
{
    ws.send(Message::text(frame.to_string())).await.unwrap();
    loop {
        let next = tokio::time::timeout(std::time::Duration::from_secs(30), ws.next())
            .await
            .expect("an answer within 30 s");
        let Some(Ok(Message::Text(text))) = next else {
            panic!("socket ended: {next:?}");
        };
        let value: Value = serde_json::from_str(text.as_str()).unwrap();
        if value.get("id") == frame.get("id") && value.get("method").is_none() {
            return value;
        }
    }
}
