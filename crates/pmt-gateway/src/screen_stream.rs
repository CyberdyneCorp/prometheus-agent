//! The bot screen's display stream (spec: execution-environments, Bot
//! screen; workspace-architecture, the RFB-over-WebSocket endpoint): `/screen`
//! on the daemon's listener, admitted with a single-use 30-second display
//! ticket (`screen.ticket`) and the `prometheus-display.v1` subprotocol,
//! relays binary WebSocket frames to and from the profile's Xvnc RFB
//! socket. Input from the viewer counts as use of the screen (its idle
//! stop) and of a person's takeover lease.

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use pmt_config::screen_lease::{self, ScreenLease};
use pmt_rpc::codes::CloseCode;
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;

use crate::server::Server;
use crate::tickets::Grant;

/// The display stream's path.
pub const PATH: &str = "/screen";
/// The display stream's subprotocol.
pub const SUBPROTOCOL: &str = "prometheus-display.v1";

/// How often viewer input refreshes the screen's and the lease's use.
const TOUCH_EVERY: Duration = Duration::from_secs(5);

/// The RFB socket of the screen of the profile at `profile_dir`, when one
/// runs.
fn rfb_socket(profile_dir: &Path) -> Option<PathBuf> {
    let record = profile_dir.join("run").join("screen").join("screen.json");
    let text = pmt_core::private_fs::read_no_follow(&record).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value["rfb"].as_str().map(PathBuf::from)
}

async fn close(mut socket: WebSocketStream<TcpStream>, code: CloseCode, reason: &str) {
    let frame = CloseFrame {
        code: code.code().into(),
        reason: reason.to_owned().into(),
    };
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        socket.send(Message::Close(Some(frame))),
    )
    .await;
}

/// Relays the screen of the ticket's profile until either side closes.
pub async fn serve(server: &Server, socket: WebSocketStream<TcpStream>, grant: Grant) {
    let (Some(home), Some(profile)) = (server.home(), grant.profiles.first()) else {
        return close(socket, CloseCode::Internal, "no profiles on disk").await;
    };
    let profile_dir = home.join("profiles").join(profile);
    let Some(rfb) = rfb_socket(&profile_dir) else {
        return close(
            socket,
            CloseCode::EndpointDisabled,
            "the bot screen is not running",
        )
        .await;
    };
    relay(socket, &rfb, &profile_dir).await;
}

#[cfg(unix)]
async fn relay(socket: WebSocketStream<TcpStream>, rfb: &Path, profile_dir: &Path) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let Ok(display) = tokio::net::UnixStream::connect(rfb).await else {
        return close(
            socket,
            CloseCode::EndpointDisabled,
            "the bot screen is not running",
        )
        .await;
    };
    let (mut from_screen, mut to_screen) = display.into_split();
    let (mut sink, mut stream) = socket.split();
    let lease = ScreenLease::of(profile_dir);
    let mut touched = tokio::time::Instant::now() - TOUCH_EVERY;
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        tokio::select! {
            read = from_screen.read(&mut buffer) => match read {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if sink.send(Message::binary(buffer[..n].to_vec())).await.is_err() {
                        break;
                    }
                }
            },
            message = stream.next() => match message {
                Some(Ok(Message::Binary(data))) => {
                    if to_screen.write_all(&data).await.is_err() {
                        break;
                    }
                    if touched.elapsed() >= TOUCH_EVERY {
                        touched = tokio::time::Instant::now();
                        screen_lease::mark_screen_used(profile_dir);
                        let _ = lease.touch_holder(screen_lease::now());
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Text(_) | Message::Frame(_))) => {}
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
            },
        }
    }
    let _ = sink.close().await;
}

#[cfg(not(unix))]
async fn relay(socket: WebSocketStream<TcpStream>, _rfb: &Path, _profile_dir: &Path) {
    close(
        socket,
        CloseCode::EndpointDisabled,
        "the bot screen needs a Unix host",
    )
    .await;
}
