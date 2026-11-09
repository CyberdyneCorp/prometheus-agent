//! The keyless `edge` speech provider: Microsoft Edge's read-aloud voices
//! over its WebSocket protocol (spec: web-and-media-tools, Built-in
//! text-to-speech providers).
//!
//! One synthesis is one connection: a `speech.config` message picks MP3
//! output, an `ssml` message carries the text, and binary `Path:audio`
//! frames bring the audio until `Path:turn.end`. Each connection carries
//! the time-bound `Sec-MS-GEC` token the service expects.

use std::fmt::Write as _;
use std::sync::Arc;

use futures_util::{SinkExt as _, StreamExt as _};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

/// The read-aloud endpoint.
pub const EDGE_URL: &str =
    "wss://speech.platform.bing.com/consumer/speech/synthesize/readaloud/edge/v1";
/// The public client token of the read-aloud service.
const TRUSTED_CLIENT_TOKEN: &str = "6A5AA1D4EAFF4E9FB37E23D68491D6F4";
const GEC_VERSION: &str = "1-140.0.3485.14";
/// Seconds between 1601-01-01 and 1970-01-01.
const WINDOWS_EPOCH: u64 = 11_644_473_600;

/// The `Sec-MS-GEC` token at `unix_secs`: SHA-256 of the 5-minute-rounded
/// Windows file time and the client token, upper-case hex.
#[must_use]
pub fn sec_ms_gec(unix_secs: u64) -> String {
    let mut ticks = unix_secs + WINDOWS_EPOCH;
    ticks -= ticks % 300;
    let file_time = u128::from(ticks) * 10_000_000;
    let digest = Sha256::digest(format!("{file_time}{TRUSTED_CLIENT_TOKEN}").as_bytes());
    digest.iter().fold(String::with_capacity(64), |mut hex, b| {
        let _ = write!(hex, "{b:02X}");
        hex
    })
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The SSML of one request.
#[must_use]
pub fn ssml(text: &str, voice: &str, speed: f64) -> String {
    #[allow(clippy::cast_possible_truncation)]
    let rate = ((speed - 1.0) * 100.0).round() as i64;
    let lang = voice.splitn(3, '-').take(2).collect::<Vec<_>>().join("-");
    format!(
        "<speak version='1.0' xmlns='http://www.w3.org/2001/10/synthesis' xml:lang='{lang}'>\
         <voice name='{}'><prosody pitch='+0Hz' rate='{rate:+}%' volume='+0%'>{}</prosody></voice></speak>",
        escape(voice),
        escape(text)
    )
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

fn timestamp() -> String {
    jiff::Timestamp::now()
        .strftime("%a %b %d %Y %H:%M:%S GMT+0000 (Coordinated Universal Time)")
        .to_string()
}

/// Synthesizes `text` with `voice` at `speed` against `base` (the
/// read-aloud endpoint or a test server): MP3 bytes.
///
/// # Errors
///
/// Connection, handshake or protocol failures; no audio.
pub async fn synthesize(
    base: &str,
    text: &str,
    voice: &str,
    speed: f64,
) -> Result<Vec<u8>, String> {
    let now = jiff::Timestamp::now().as_second().try_into().unwrap_or(0);
    let connection = ulid::Ulid::generate().to_string().to_lowercase();
    let url = format!(
        "{base}?TrustedClientToken={TRUSTED_CLIENT_TOKEN}&Sec-MS-GEC={}&Sec-MS-GEC-Version={GEC_VERSION}&ConnectionId={connection}",
        sec_ms_gec(now)
    );
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|e| format!("invalid edge URL: {e}"))?;
    let headers = request.headers_mut();
    headers.insert(
        "Origin",
        "chrome-extension://jdiccldimpdaibmpdkjnbmckianbfold"
            .parse()
            .map_err(|_| "bad header")?,
    );
    let mut socket = dial(request).await?;
    let config = format!(
        "X-Timestamp:{}\r\nContent-Type:application/json; charset=utf-8\r\nPath:speech.config\r\n\r\n\
         {{\"context\":{{\"synthesis\":{{\"audio\":{{\"metadataoptions\":{{\"sentenceBoundaryEnabled\":\"false\",\"wordBoundaryEnabled\":\"false\"}},\"outputFormat\":\"audio-24khz-48kbitrate-mono-mp3\"}}}}}}}}",
        timestamp()
    );
    let speak = format!(
        "X-RequestId:{connection}\r\nContent-Type:application/ssml+xml\r\nX-Timestamp:{}Z\r\nPath:ssml\r\n\r\n{}",
        timestamp(),
        ssml(text, voice, speed)
    );
    for message in [config, speak] {
        socket
            .send(Message::text(message))
            .await
            .map_err(|e| format!("edge: {e}"))?;
    }
    read_audio(&mut socket).await
}

async fn read_audio<S>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
) -> Result<Vec<u8>, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut audio = Vec::new();
    while let Some(frame) = socket.next().await {
        match frame.map_err(|e| format!("edge: {e}"))? {
            Message::Binary(data) if data.len() >= 2 => {
                let header_len = usize::from(u16::from_be_bytes([data[0], data[1]]));
                let header = data.get(2..2 + header_len).unwrap_or_default();
                if header.windows(10).any(|w| w == b"Path:audio") {
                    audio.extend_from_slice(data.get(2 + header_len..).unwrap_or_default());
                }
            }
            Message::Text(text) if text.contains("Path:turn.end") => break,
            Message::Close(_) => break,
            _ => {}
        }
    }
    if audio.is_empty() {
        return Err("edge returned no audio (unknown voice?)".to_owned());
    }
    Ok(audio)
}

async fn dial(
    request: tokio_tungstenite::tungstenite::handshake::client::Request,
) -> Result<tokio_tungstenite::WebSocketStream<Box<dyn Io>>, String> {
    let uri = request.uri().clone();
    let host = uri.host().ok_or("edge URL has no host")?.to_owned();
    let tls = uri.scheme_str() == Some("wss");
    let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
    let tcp = TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|e| format!("cannot reach {host}:{port}: {e}"))?;
    let stream: Box<dyn Io> = if tls {
        let config = pmt_net::tls::client_config(None).map_err(|e| e.to_string())?;
        let name = ServerName::try_from(host.clone())
            .map_err(|_| format!("{host} is not a valid TLS name"))?;
        Box::new(
            TlsConnector::from(Arc::new(config))
                .connect(name, tcp)
                .await
                .map_err(|e| format!("TLS with {host} failed: {e}"))?,
        )
    } else {
        Box::new(tcp)
    };
    let (socket, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .map_err(|e| format!("the edge handshake failed: {e}"))?;
    Ok(socket)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_is_stable_within_five_minutes() {
        let a = sec_ms_gec(1_762_700_000);
        assert_eq!(a.len(), 64);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase())
        );
        assert_eq!(a, sec_ms_gec(1_762_700_000 + 1));
        let rounded = 1_762_700_000 - (1_762_700_000 + WINDOWS_EPOCH) % 300;
        assert_ne!(a, sec_ms_gec(rounded + 300));
    }

    #[test]
    fn ssml_escapes_text_and_sets_the_rate() {
        let doc = ssml("a < b & c", "en-US-AriaNeural", 1.25);
        assert!(doc.contains("xml:lang='en-US'"));
        assert!(doc.contains("rate='+25%'"));
        assert!(doc.contains("a &lt; b &amp; c"));
    }
}
