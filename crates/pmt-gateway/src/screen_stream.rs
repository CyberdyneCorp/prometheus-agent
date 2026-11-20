//! The bot screen's display stream (spec: execution-environments, Bot
//! screen; workspace-architecture, the RFB-over-WebSocket endpoint): `/screen`
//! on the daemon's listener, admitted with a single-use 30-second display
//! ticket (`screen.ticket`) and the `prometheus-display.v1` subprotocol,
//! relays binary WebSocket frames to and from the profile's Xvnc RFB
//! socket. Input from the viewer counts as use of the screen (its idle
//! stop) and of a person's takeover lease. An observer's stream is
//! read-only: its input never reaches the screen
//! ([`crate::screen_observer`]) and does not count as use.

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use pmt_config::screen_lease::{self, ScreenLease};
use pmt_rpc::codes::CloseCode;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;

use crate::connection::Transport;
use crate::screen_observer::ObserverFilter;
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

async fn close<S: Transport>(mut socket: WebSocketStream<S>, code: CloseCode, reason: &str) {
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
pub async fn serve<S: Transport>(server: &Server, socket: WebSocketStream<S>, grant: Grant) {
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
    relay(socket, &rfb, &profile_dir, grant.observer_only).await;
}

/// The viewer's side of the relay: what it may send and whether its input
/// counts as use.
struct Viewer {
    observer: Option<ObserverFilter>,
    touched: tokio::time::Instant,
    lease: ScreenLease,
}

impl Viewer {
    /// The bytes of `data` the screen receives; `Err` closes the stream.
    fn admit(&mut self, data: &[u8], profile_dir: &Path) -> Result<Vec<u8>, &'static str> {
        if let Some(filter) = &mut self.observer {
            return filter.feed(data);
        }
        if self.touched.elapsed() >= TOUCH_EVERY {
            self.touched = tokio::time::Instant::now();
            screen_lease::mark_screen_used(profile_dir);
            let _ = self.lease.touch_holder(screen_lease::now());
        }
        Ok(data.to_vec())
    }
}

#[cfg(unix)]
async fn relay<S: Transport>(
    socket: WebSocketStream<S>,
    rfb: &Path,
    profile_dir: &Path,
    observer: bool,
) {
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
    let mut viewer = Viewer {
        observer: observer.then(ObserverFilter::new),
        touched: tokio::time::Instant::now() - TOUCH_EVERY,
        lease: ScreenLease::of(profile_dir),
    };
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
                Some(Ok(Message::Binary(data))) => match viewer.admit(&data, profile_dir) {
                    Ok(bytes) => {
                        if to_screen.write_all(&bytes).await.is_err() {
                            break;
                        }
                    }
                    Err(why) => {
                        let frame = CloseFrame {
                            code: CloseCode::NotAllowed.code().into(),
                            reason: why.into(),
                        };
                        let _ = sink.send(Message::Close(Some(frame))).await;
                        break;
                    }
                },
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Text(_) | Message::Frame(_))) => {}
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
            },
        }
    }
    let _ = sink.close().await;
}

#[cfg(not(unix))]
async fn relay<S: Transport>(
    socket: WebSocketStream<S>,
    _rfb: &Path,
    _profile_dir: &Path,
    _observer: bool,
) {
    close(
        socket,
        CloseCode::EndpointDisabled,
        "the bot screen needs a Unix host",
    )
    .await;
}

#[cfg(all(test, unix))]
mod tests {
    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio::io::AsyncReadExt as _;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::protocol::Role;

    /// Everything a viewer sends through the relay, as the screen's RFB
    /// socket receives it, and whether the screen counted it as use.
    async fn screen_receives(observer: bool, sent: &[&[u8]]) -> (Vec<u8>, bool) {
        let dir = tempfile::tempdir().unwrap();
        let rfb = dir.path().join("rfb");
        let listener = tokio::net::UnixListener::bind(&rfb).unwrap();
        let marker = dir.path().join("run/screen/used");
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        let long_ago = std::time::SystemTime::now() - std::time::Duration::from_hours(1);
        std::fs::File::create(&marker)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        let (server_end, client_end) = tokio::io::duplex(64 * 1024);
        let server = WebSocketStream::from_raw_socket(server_end, Role::Server, None).await;
        let mut client = WebSocketStream::from_raw_socket(client_end, Role::Client, None).await;
        let profile = dir.path().to_owned();
        let relay = pmt_core::spawn::spawn(async move {
            super::relay(server, &rfb, &profile, observer).await;
        });
        let (mut screen, _) = listener.accept().await.unwrap();
        for chunk in sent {
            client.send(Message::binary(chunk.to_vec())).await.unwrap();
        }
        client.close(None).await.unwrap();
        while client.next().await.is_some() {}
        relay.await.unwrap();
        let mut received = Vec::new();
        screen.read_to_end(&mut received).await.unwrap();
        let used = std::fs::metadata(&marker).unwrap().modified().unwrap() > long_ago;
        (received, used)
    }

    const POINTER: &[u8] = &[5, 1, 0, 10, 0, 20];

    /// Spec: tui, observers are read-only: an observer watching the bot
    /// screen sends a pointer event; the screen never receives it and the
    /// stream does not count as use.
    #[tokio::test]
    async fn an_observers_pointer_event_never_reaches_the_screen() {
        let (received, used) = screen_receives(
            true,
            &[
                b"RFB 003.008\n",
                &[1, 0],
                &[3, 0, 0, 0, 0, 0, 0, 8, 0, 8],
                POINTER,
            ],
        )
        .await;
        assert_eq!(received, b"RFB 003.008\n\x01\x01\x03\0\0\0\0\0\0\x08\0\x08");
        assert!(!used);
    }

    #[tokio::test]
    async fn a_controlling_viewers_input_reaches_the_screen_and_counts_as_use() {
        let (received, used) = screen_receives(false, &[b"RFB 003.008\n", &[1, 0], POINTER]).await;
        assert_eq!(
            received,
            [b"RFB 003.008\n".as_slice(), &[1, 0], POINTER].concat()
        );
        assert!(used);
    }
}
