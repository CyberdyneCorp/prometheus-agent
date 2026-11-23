//! Adapter host forwarding mode (spec: platform-adapters, Adapter host
//! forwarding mode): `daemon run --adapters-only --upstream <ws-url>` runs
//! only the configured adapters. Each event they admit (sender
//! authorization, pairing, the bot-loop guard and deduplication stay
//! here) is forwarded to the upstream daemon, which owns every session; the
//! upstream's sends, edits and media reads come back and run on the local
//! adapters. The host dials the upstream's remote attachment listener at
//! `/adapters` with the admin device token in the secret named by
//! `daemon.upstream_ticket_secret` and speaks the relay connector's side of
//! its wire ([`super::relay`]). While the upstream cannot be reached the
//! local adapters are `degraded` (`upstream_unreachable`) and a person
//! writing in is told the agent is unavailable.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use base64::Engine as _;
use futures_util::{SinkExt as _, StreamExt as _};
use pmt_core::secret::SecretString;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tracing::Instrument as _;

use super::Shared;
use super::adapter::{Admission, Attachment, Inbound, OutboundAttachment, SendError};
use super::relay;

/// How long a forwarded event waits for the upstream's admission.
const ADMISSION_TIMEOUT: Duration = Duration::from_secs(30);
const RECONNECT_START: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_mins(1);
/// Inbound attachments the upstream may still ask for.
const MEDIA_KEPT: usize = 1024;

/// Where the host forwards to (`daemon run --upstream`, the token from the
/// launch profile's secret `daemon.upstream_ticket_secret`).
#[derive(Clone)]
pub struct ForwardOptions {
    /// The upstream's `/adapters` WebSocket URL (`wss://`; `ws://` only to
    /// a loopback host).
    pub url: String,
    /// The admin device token the upstream paired for this host.
    pub token: SecretString,
    /// Extra PEM roots (`network.extra_ca_file` of the launch profile),
    /// for an upstream with its own certificate.
    pub extra_ca_file: Option<PathBuf>,
}

impl std::fmt::Debug for ForwardOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForwardOptions")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

/// An inbound attachment handed to the upstream as an opaque location.
struct Kept {
    profile: String,
    platform: String,
    attachment: Attachment,
}

/// The connection to the upstream.
pub(crate) struct Forwarder {
    options: ForwardOptions,
    shared: OnceLock<Weak<Shared>>,
    out: Mutex<Option<mpsc::UnboundedSender<Message>>>,
    /// Why the upstream is unreachable; `None` while connected.
    down: watch::Sender<Option<String>>,
    /// Forwarded events waiting for their admission, by message id.
    pending: Mutex<HashMap<String, oneshot::Sender<Admission>>>,
    media: Mutex<(HashMap<String, Kept>, VecDeque<String>)>,
    next: AtomicU64,
}

impl Forwarder {
    /// Why the upstream cannot be reached now, if it cannot.
    pub(crate) fn down_reason(&self) -> Option<String> {
        self.down.borrow().clone()
    }

    fn send_frame(&self, frame: &Value) -> bool {
        self.out
            .lock()
            .expect("forwarder poisoned")
            .as_ref()
            .is_some_and(|out| out.send(Message::text(frame.to_string())).is_ok())
    }

    /// Forwards an admitted event of `profile`'s `platform`; `None` when
    /// the upstream is unreachable.
    pub(crate) async fn forward(
        &self,
        profile: &str,
        platform: &str,
        mut inbound: Inbound,
    ) -> Option<Admission> {
        if self.down_reason().is_some() {
            return None;
        }
        for attachment in &mut inbound.attachments {
            attachment.location = self.keep(profile, platform, attachment.clone());
        }
        let message_id = inbound.message_id.clone();
        let (tx, rx) = oneshot::channel();
        if !message_id.is_empty() {
            self.pending
                .lock()
                .expect("forwarder poisoned")
                .insert(message_id.clone(), tx);
        }
        if !self.send_frame(&relay::inbound_frame(profile, platform, &inbound)) {
            self.pending
                .lock()
                .expect("forwarder poisoned")
                .remove(&message_id);
            return None;
        }
        if message_id.is_empty() {
            return Some(Admission::Accepted);
        }
        if let Ok(Ok(admission)) = tokio::time::timeout(ADMISSION_TIMEOUT, rx).await {
            return Some(admission);
        }
        // Sent: the upstream has it even if its answer was lost.
        self.pending
            .lock()
            .expect("forwarder poisoned")
            .remove(&message_id);
        Some(Admission::Accepted)
    }

    /// Remembers an attachment and returns the location the upstream asks
    /// for it by.
    fn keep(&self, profile: &str, platform: &str, attachment: Attachment) -> String {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let location = format!("fwd:{n}:{}", crate::tickets::random_hex(8));
        let mut media = self.media.lock().expect("forwarder poisoned");
        let (kept, order) = &mut *media;
        kept.insert(
            location.clone(),
            Kept {
                profile: profile.to_owned(),
                platform: platform.to_owned(),
                attachment,
            },
        );
        order.push_back(location.clone());
        while order.len() > MEDIA_KEPT {
            if let Some(old) = order.pop_front() {
                kept.remove(&old);
            }
        }
        location
    }

    fn set_down(&self, shared: &Shared, why: Option<String>) {
        *self.out.lock().expect("forwarder poisoned") = None;
        self.pending.lock().expect("forwarder poisoned").clear();
        let reason = why.unwrap_or_else(|| "not connected yet".to_owned());
        self.down.send_replace(Some(reason.clone()));
        shared.adapters.upstream_changed(false, &reason);
    }
}

/// Starts forwarding to the upstream the gateway's options name, if any.
pub(crate) fn start(shared: &Arc<Shared>) {
    let Some(options) = shared.options.forward.clone() else {
        return;
    };
    let forwarder = Arc::new(Forwarder {
        options,
        shared: OnceLock::new(),
        out: Mutex::default(),
        down: watch::channel(Some("not connected yet".to_owned())).0,
        pending: Mutex::default(),
        media: Mutex::default(),
        next: AtomicU64::new(1),
    });
    let _ = forwarder.shared.set(Arc::downgrade(shared));
    if shared.forwarder.set(Arc::clone(&forwarder)).is_err() {
        return;
    }
    // Adapters start first (the upstream gets their descriptors on
    // connect), so one may already report `connected`: it is only as good
    // as an upstream not reached yet.
    shared.adapters.upstream_changed(false, "not connected yet");
    let span = tracing::info_span!("forward", upstream = forwarder.options.url);
    pmt_core::spawn::spawn(supervise(forwarder).instrument(span));
}

async fn supervise(forwarder: Arc<Forwarder>) {
    let mut wait = RECONNECT_START;
    loop {
        let Some(shared) = forwarder.shared.get().and_then(Weak::upgrade) else {
            return;
        };
        let stop = shared.stop.clone();
        let outcome = tokio::select! {
            () = stop.cancelled() => return,
            outcome = session(&forwarder, &shared) => outcome,
        };
        let why = match outcome {
            Ok(()) => {
                wait = RECONNECT_START;
                "the upstream closed the connection".to_owned()
            }
            Err(why) => why,
        };
        tracing::warn!(
            "upstream daemon: {why}; reconnecting in {}s",
            wait.as_secs()
        );
        forwarder.set_down(&shared, Some(why));
        drop(shared);
        tokio::select! {
            () = stop.cancelled() => return,
            () = tokio::time::sleep(wait) => {}
        }
        wait = (wait * 2).min(RECONNECT_MAX);
    }
}

/// One connection: the descriptor of the local adapters, then frames
/// until it closes.
async fn session(forwarder: &Arc<Forwarder>, shared: &Arc<Shared>) -> Result<(), String> {
    let socket = dial(&forwarder.options).await?;
    let (mut sink, mut stream) = socket.split();
    let descriptor = relay::descriptor_frame(&shared.adapters.descriptors());
    sink.send(Message::text(descriptor.to_string()))
        .await
        .map_err(|e| format!("the upstream connection failed: {e}"))?;
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    *forwarder.out.lock().expect("forwarder poisoned") = Some(tx);
    forwarder.down.send_replace(None);
    shared.adapters.upstream_changed(true, "");
    tracing::info!("forwarding to the upstream daemon");
    let writer = pmt_core::spawn::spawn(async move {
        while let Some(message) = rx.recv().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
    });
    let result = loop {
        match stream.next().await {
            None | Some(Ok(Message::Close(_))) => break Ok(()),
            Some(Ok(Message::Text(text))) => match serde_json::from_str::<Value>(&text) {
                Ok(frame) => handle(forwarder, shared, frame),
                Err(e) => break Err(format!("bad frame from the upstream: {e}")),
            },
            Some(Ok(_)) => {}
            Some(Err(e)) => break Err(format!("the upstream connection failed: {e}")),
        }
    };
    writer.abort();
    result
}

async fn dial(options: &ForwardOptions) -> Result<super::ws::Socket, String> {
    if !pmt_config::daemon_relay::secure_url(&options.url, "wss", "ws") {
        return Err(format!(
            "refusing to send the upstream ticket to {}: use wss:// (ws:// only to a loopback host)",
            options.url
        ));
    }
    let mut request = options
        .url
        .as_str()
        .into_client_request()
        .map_err(|e| format!("bad upstream URL {}: {e}", options.url))?;
    let bearer = HeaderValue::from_str(&format!("Bearer {}", options.token.expose()))
        .map_err(|_| "the upstream ticket is not a valid header value".to_owned())?;
    request.headers_mut().insert("Authorization", bearer);
    super::ws::connect(request, options.extra_ca_file.as_deref())
        .await
        .map_err(|e| format!("the upstream connection failed: {e}"))
}

fn handle(forwarder: &Arc<Forwarder>, shared: &Arc<Shared>, frame: Value) {
    match frame["type"].as_str() {
        Some("admission") => {
            let id = frame["message_id"].as_str().unwrap_or_default();
            let waiter = forwarder
                .pending
                .lock()
                .expect("forwarder poisoned")
                .remove(id);
            let admission =
                serde_json::from_value(frame["admission"].clone()).unwrap_or(Admission::Accepted);
            if let Some(waiter) = waiter {
                let _ = waiter.send(admission);
            }
        }
        Some(kind @ ("send" | "edit" | "egress" | "media")) => {
            let kind = kind.to_owned();
            let (forwarder, shared) = (Arc::clone(forwarder), Arc::clone(shared));
            pmt_core::spawn::spawn(
                async move {
                    let answer = answer(&forwarder, &shared, &kind, &frame).await;
                    forwarder.send_frame(&answer);
                }
                .in_current_span(),
            );
        }
        // A newer upstream's frame this build does not know.
        _ => {}
    }
}

/// The result frame of one upstream request.
async fn answer(forwarder: &Forwarder, shared: &Shared, kind: &str, frame: &Value) -> Value {
    let id = &frame["id"];
    let platform = frame["platform"].as_str().unwrap_or_default();
    let outcome = match kind {
        "egress" => Ok(json!({"allow": shared.adapters.runs(platform)})),
        "media" => media(forwarder, shared, frame).await,
        _ => send(shared, frame).await,
    };
    match outcome {
        Ok(extra) => relay::ok_result(id, extra),
        Err(error) => relay::error_result(id, &error),
    }
}

/// The profile a request is for: the one it names, else the only one
/// running the platform.
fn profile_of(shared: &Shared, frame: &Value) -> Option<String> {
    if let Some(profile) = frame["profile"].as_str() {
        return Some(profile.to_owned());
    }
    let platform = frame["platform"].as_str()?;
    shared.server.profiles().into_iter().find(|p| {
        shared
            .adapters
            .statuses()
            .iter()
            .any(|s| s.profile == *p && s.platform == platform)
    })
}

async fn send(shared: &Shared, frame: &Value) -> Result<Value, SendError> {
    let platform = frame["platform"].as_str().unwrap_or_default();
    let profile = profile_of(shared, frame)
        .ok_or_else(|| SendError::Permanent(format!("no local adapter runs {platform}")))?;
    let mut message = relay::parse_outgoing(frame)
        .ok_or_else(|| SendError::Permanent("the request names no chat".to_owned()))?;
    let scratch = scratch_dir(&shared.options.home);
    let files = write_carried(&scratch, &frame["attachments"]).await?;
    message.attachments = files.paths;
    let edit_of = (frame["type"] == "edit")
        .then(|| frame["message_id"].as_str())
        .flatten();
    let sent = shared
        .adapters
        .send_rendered(&profile, platform, &message, edit_of)
        .await;
    drop(files.dir);
    sent.map(|message_id| json!({"message_id": message_id}))
}

async fn media(forwarder: &Forwarder, shared: &Shared, frame: &Value) -> Result<Value, SendError> {
    let location = frame["location"].as_str().unwrap_or_default();
    let kept = {
        let media = forwarder.media.lock().expect("forwarder poisoned");
        media
            .0
            .get(location)
            .map(|k| (k.profile.clone(), k.platform.clone(), k.attachment.clone()))
    };
    let Some((profile, platform, attachment)) = kept else {
        return Err(SendError::Permanent(
            "the attachment is no longer available on the adapter host".to_owned(),
        ));
    };
    let mut stream = shared
        .adapters
        .open_media(&profile, &platform, &attachment)
        .await?;
    let mut data = Vec::new();
    while let Some(chunk) = stream.chunks.next().await {
        data.extend(chunk.map_err(SendError::Network)?);
        if data.len() > relay::MAX_CARRIED_BYTES {
            return Err(SendError::Permanent(format!(
                "the attachment is larger than the {} MiB the upstream may read",
                relay::MAX_CARRIED_BYTES / (1024 * 1024)
            )));
        }
    }
    Ok(json!({"data": base64::engine::general_purpose::STANDARD.encode(data)}))
}

fn scratch_dir(home: &Path) -> PathBuf {
    home.join("cache").join("forwarded")
}

/// Carried files written for one send; removed when dropped.
struct Written {
    dir: Option<tempfile::TempDir>,
    paths: Vec<OutboundAttachment>,
}

async fn write_carried(scratch: &Path, carried: &Value) -> Result<Written, SendError> {
    let Some(list) = carried.as_array().filter(|l| !l.is_empty()) else {
        return Ok(Written {
            dir: None,
            paths: Vec::new(),
        });
    };
    let failed =
        |e: std::io::Error| SendError::Permanent(format!("cannot stage an attachment: {e}"));
    tokio::fs::create_dir_all(scratch).await.map_err(failed)?;
    let dir = tempfile::Builder::new()
        .prefix("send-")
        .tempdir_in(scratch)
        .map_err(failed)?;
    let mut paths = Vec::with_capacity(list.len());
    for (n, entry) in list.iter().enumerate() {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(entry["data"].as_str().unwrap_or_default())
            .map_err(|e| SendError::Permanent(format!("bad attachment data: {e}")))?;
        // Only the final component of the sender's name is kept.
        let name = entry["name"]
            .as_str()
            .and_then(|n| Path::new(n).file_name())
            .map_or_else(
                || format!("attachment-{n}"),
                |n| n.to_string_lossy().into_owned(),
            );
        let path = dir.path().join(format!("{n}-{name}"));
        tokio::fs::write(&path, bytes).await.map_err(failed)?;
        paths.push(OutboundAttachment {
            path: path.to_string_lossy().into_owned(),
            as_document: entry["as_document"].as_bool().unwrap_or(false),
            as_voice: entry["as_voice"].as_bool().unwrap_or(false),
        });
    }
    Ok(Written {
        dir: Some(dir),
        paths,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn carried_files_keep_only_their_name_and_vanish_after_the_send() {
        let scratch = tempfile::tempdir().unwrap();
        let carried = json!([{
            "name": "../../etc/passwd",
            "as_voice": true,
            "data": base64::engine::general_purpose::STANDARD.encode(b"voice"),
        }]);
        let written = write_carried(scratch.path(), &carried).await.unwrap();
        let path = PathBuf::from(&written.paths[0].path);
        assert!(path.starts_with(scratch.path()), "{}", path.display());
        assert_eq!(path.file_name().unwrap(), "0-passwd");
        assert_eq!(std::fs::read(&path).unwrap(), b"voice");
        assert!(written.paths[0].as_voice);
        drop(written);
        assert!(!path.exists());
    }
}
