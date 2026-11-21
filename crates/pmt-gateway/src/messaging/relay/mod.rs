//! The relay connector (spec: messaging-gateway, Relay connector): when
//! `daemon.toml gateway.relay.url` is set the daemon dials the connector
//! over TLS with the credential from the secrets store, reads its
//! capability descriptor and serves every platform it fronts through it,
//! for every served profile: the connector authorizes senders upstream
//! (`authorization_is_upstream`), and a profile's direct adapter of a
//! fronted platform is not connected unless `allow_direct_platforms`
//! lists it. The connection is supervised: a loss marks the relayed
//! platforms `retrying` and the daemon reconnects with backoff.
//!
//! The same wire serves a forwarding adapter host (spec: platform-adapters,
//! Adapter host forwarding mode): a daemon run with `--adapters-only`
//! dials this daemon's remote attachment listener at `/adapters` with an
//! admin device token and speaks the connector's side
//! ([`serve_forwarder`]); it vouches for its senders like a connector,
//! may address only the profiles its device is bound to, and reconnects
//! by itself.

mod dial;
mod view;
mod wire;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tracing::Instrument as _;

use super::Shared;
use super::adapter::{AdapterError, AdapterLink, AdapterState};

pub(crate) use view::RelayView;
pub(crate) use wire::{
    RelayDescriptor, descriptor_frame, error_result, inbound_frame, ok_result, parse_outgoing,
};

/// How long the daemon waits for the descriptor at start before it starts
/// direct adapters (a later descriptor still takes their platforms over).
pub(crate) const STARTUP_WAIT: Duration = Duration::from_secs(5);
/// How long a request waits for its result.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the connector has to send its descriptor.
const DESCRIPTOR_TIMEOUT: Duration = Duration::from_secs(10);
const RECONNECT_START: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_mins(5);
/// The largest attachment carried over the wire, either way.
pub(crate) const MAX_CARRIED_BYTES: usize = 25 * 1024 * 1024;

/// How the daemon reaches the connector (all from `daemon.toml` and the
/// launch profile's secrets).
#[derive(Clone)]
pub struct RelayOptions {
    /// The connector's WebSocket URL.
    pub url: String,
    /// The gateway credential.
    pub token: SecretString,
    /// This gateway's id at the connector.
    pub gateway_id: Option<String>,
    /// Fronted platforms whose direct adapters still connect.
    pub allow_direct_platforms: Vec<String>,
    /// The profile of events that name none (the launch profile).
    pub default_profile: String,
    /// Extra PEM roots (`network.extra_ca_file` of the launch profile).
    pub extra_ca_file: Option<PathBuf>,
}

impl std::fmt::Debug for RelayOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayOptions")
            .field("url", &self.url)
            .field("gateway_id", &self.gateway_id)
            .field("allow_direct_platforms", &self.allow_direct_platforms)
            .finish_non_exhaustive()
    }
}

/// Why a request got no result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unanswered {
    /// Never left: the relay is not connected.
    NotSent,
    /// The connection dropped after it left.
    Dropped,
    /// No result in time.
    TimedOut,
}

impl Unanswered {
    fn describe(self) -> &'static str {
        match self {
            Self::NotSent => "the relay is not connected",
            Self::Dropped => "the relay connection dropped",
            Self::TimedOut => "the relay did not answer in time",
        }
    }
}

/// The live connection to the connector.
pub(crate) struct Relay {
    options: RelayOptions,
    /// Served for a forwarding adapter host that attaches (never dialed).
    attached: bool,
    /// The profiles the attached host's device may address.
    grant: Mutex<Vec<String>>,
    /// An attached host is connected (a second one is refused).
    busy: AtomicBool,
    shared: OnceLock<Weak<Shared>>,
    /// Frames to send while connected.
    out: Mutex<Option<mpsc::UnboundedSender<Message>>>,
    descriptor: Mutex<Option<RelayDescriptor>>,
    connected: watch::Sender<bool>,
    pending: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    /// Links of the relayed platforms' slots, told when the connection drops.
    links: Mutex<Vec<Arc<dyn AdapterLink>>>,
    next: AtomicU64,
}

impl Relay {
    /// The connector's URL.
    pub(crate) fn url(&self) -> &str {
        &self.options.url
    }

    /// Whether this serves an attached forwarding adapter host.
    pub(crate) fn is_attached(&self) -> bool {
        self.attached
    }

    pub(crate) fn new(options: RelayOptions) -> Arc<Self> {
        Self::build(options, false)
    }

    /// The relay of forwarding adapter hosts that attach; events naming no
    /// profile belong to `default_profile`.
    fn for_forwarders(default_profile: String) -> Arc<Self> {
        let options = RelayOptions {
            url: "the forwarding adapter host".to_owned(),
            token: SecretString::new(String::new()),
            gateway_id: None,
            allow_direct_platforms: Vec::new(),
            default_profile,
            extra_ca_file: None,
        };
        Self::build(options, true)
    }

    fn build(options: RelayOptions, attached: bool) -> Arc<Self> {
        Arc::new(Self {
            options,
            attached,
            grant: Mutex::default(),
            busy: AtomicBool::new(false),
            shared: OnceLock::new(),
            out: Mutex::default(),
            descriptor: Mutex::default(),
            connected: watch::channel(false).0,
            pending: Mutex::default(),
            links: Mutex::default(),
            next: AtomicU64::new(1),
        })
    }

    /// The connector's descriptor, once received.
    pub(crate) fn descriptor(&self) -> Option<RelayDescriptor> {
        self.descriptor.lock().expect("relay poisoned").clone()
    }

    /// Whether the relay fronts `platform` (as far as it has said).
    pub(crate) fn fronts(&self, platform: &str) -> bool {
        self.descriptor()
            .is_some_and(|d| d.platforms.iter().any(|p| p == platform))
    }

    /// Whether `platform`'s direct adapter must stay off.
    pub(crate) fn replaces_direct(&self, platform: &str) -> bool {
        self.fronts(platform)
            && !self
                .options
                .allow_direct_platforms
                .iter()
                .any(|p| p.eq_ignore_ascii_case(platform))
    }

    /// Waits up to `limit` for the first descriptor.
    pub(crate) async fn wait_descriptor(&self, limit: Duration) {
        let mut rx = self.connected.subscribe();
        let _ = tokio::time::timeout(limit, rx.wait_for(|up| *up)).await;
    }

    /// Waits until the connection is up.
    pub(crate) async fn wait_connected(&self) {
        let mut rx = self.connected.subscribe();
        let _ = rx.wait_for(|up| *up).await;
    }

    fn is_connected(&self) -> bool {
        *self.connected.borrow()
    }

    pub(crate) fn watch_link(&self, link: Arc<dyn AdapterLink>) {
        self.links.lock().expect("relay poisoned").push(link);
    }

    /// Sends a request frame and waits for its result.
    async fn request(&self, frame_of: impl FnOnce(&str) -> Value) -> Result<Value, Unanswered> {
        let id = format!("r{}", self.next.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("relay poisoned")
            .insert(id.clone(), tx);
        let sent = self
            .out
            .lock()
            .expect("relay poisoned")
            .as_ref()
            .is_some_and(|out| out.send(Message::text(frame_of(&id).to_string())).is_ok());
        if !sent {
            self.pending.lock().expect("relay poisoned").remove(&id);
            return Err(Unanswered::NotSent);
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => Err(Unanswered::Dropped),
            Err(_) => {
                self.pending.lock().expect("relay poisoned").remove(&id);
                Err(Unanswered::TimedOut)
            }
        }
    }

    /// Asks the connector whether the gateway may send to `chat_id` of
    /// `platform` (its egress authorization). Any failure is an error,
    /// which callers treat as a refusal.
    ///
    /// # Errors
    ///
    /// `gateway.relay` when the connector cannot be asked or answers
    /// without a verdict.
    pub(crate) async fn authorize_egress(
        &self,
        platform: &str,
        chat_id: &str,
    ) -> Result<bool, Error> {
        let failed = |why: &str| {
            Error::new(
                &codes::GATEWAY_RELAY,
                format!("relay egress check for {platform} failed: {why}"),
            )
        };
        let result = self
            .request(|id| wire::egress(id, platform, chat_id))
            .await
            .map_err(|e| failed(e.describe()))?;
        if result["ok"].as_bool() != Some(true) {
            return Err(failed(
                result["error"]["message"].as_str().unwrap_or("refused"),
            ));
        }
        result["allow"]
            .as_bool()
            .ok_or_else(|| failed("the answer carries no verdict"))
    }

    /// The connection went away: pending requests fail, relayed platforms
    /// report `retrying`.
    fn down(&self, why: &str) {
        *self.out.lock().expect("relay poisoned") = None;
        self.connected.send_replace(false);
        self.pending.lock().expect("relay poisoned").clear();
        let links = std::mem::take(&mut *self.links.lock().expect("relay poisoned"));
        let error = AdapterError {
            retryable: true,
            code: "relay_down".to_owned(),
            message: format!("the relay connector is unreachable: {why}"),
        };
        for link in links {
            link.report(AdapterState::Retrying, Some(error.clone()));
        }
    }
}

/// Reads the relay settings (from `daemon.toml` and the launch profile's
/// secrets); `Ok(None)` when no relay is configured.
pub type RelaySource = Arc<dyn Fn() -> Result<Option<RelayOptions>, Error> + Send + Sync>;

/// Starts the relay's supervised connection and waits briefly for its
/// descriptor, so direct adapters of fronted platforms never connect.
pub(crate) async fn start(shared: &Arc<Shared>, options: RelayOptions) {
    let relay = Relay::new(options);
    if shared.relay.set(Arc::clone(&relay)).is_err() {
        return;
    }
    let _ = relay.shared.set(Arc::downgrade(shared));
    let span = tracing::info_span!("relay");
    pmt_core::spawn::spawn(supervise(Arc::clone(&relay)).instrument(span));
    relay.wait_descriptor(STARTUP_WAIT).await;
}

/// `gateway reload`: connects a relay enrolled since the daemon started
/// (a changed relay of a running daemon applies after a restart).
pub(crate) fn reload(shared: &Arc<Shared>) {
    if shared.relay.get().is_some() {
        return;
    }
    let Some(source) = &shared.options.relay_source else {
        return;
    };
    match source() {
        Ok(Some(options)) => {
            tracing::info!(
                url = options.url,
                "connecting the relay enrolled since start"
            );
            let shared = Arc::clone(shared);
            pmt_core::spawn::spawn(async move { start(&shared, options).await }.in_current_span());
        }
        Ok(None) => {}
        Err(err) => tracing::error!("relay not connected: {err}"),
    }
}

async fn supervise(relay: Arc<Relay>) {
    let mut wait = RECONNECT_START;
    loop {
        let stop = match relay.shared.get().and_then(Weak::upgrade) {
            Some(shared) => shared.stop.clone(),
            None => return,
        };
        let outcome = tokio::select! {
            () = stop.cancelled() => return,
            outcome = session(&relay) => outcome,
        };
        let why = match outcome {
            Ok(()) => {
                wait = RECONNECT_START;
                "the connector closed the connection".to_owned()
            }
            Err(why) => why,
        };
        tracing::warn!(
            "relay connector: {why}; reconnecting in {}s",
            wait.as_secs()
        );
        relay.down(&why);
        tokio::select! {
            () = stop.cancelled() => return,
            () = tokio::time::sleep(wait) => {}
        }
        wait = (wait * 2).min(RECONNECT_MAX);
    }
}

/// One dialed connection: descriptor, then frames until it closes.
async fn session(relay: &Arc<Relay>) -> Result<(), String> {
    let options = &relay.options;
    let socket = dial::dial(
        &options.url,
        &options.token,
        options.gateway_id.as_deref(),
        options.extra_ca_file.as_deref(),
    )
    .await?;
    run(relay, socket).await
}

/// Serves a forwarding adapter host attached on the remote listener with
/// a device bound to `profiles`, until its connection closes.
///
/// # Errors
///
/// Why it was refused (this daemon dials a relay connector, or another
/// host is attached) or why the connection ended.
pub(crate) async fn serve_forwarder<S>(
    shared: &Arc<Shared>,
    socket: WebSocketStream<S>,
    profiles: Vec<String>,
) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let relay = if let Some(relay) = shared.relay.get() {
        Arc::clone(relay)
    } else {
        let default = profiles.first().cloned().unwrap_or_default();
        let fresh = Relay::for_forwarders(default);
        let _ = fresh.shared.set(Arc::downgrade(shared));
        // A concurrent attach may have won: use whichever is set.
        let _ = shared.relay.set(fresh);
        Arc::clone(shared.relay.get().expect("set above"))
    };
    if !relay.attached {
        return Err("this daemon dials a relay connector (gateway.relay.url); it does not also serve a forwarding adapter host".to_owned());
    }
    if relay.busy.swap(true, Ordering::AcqRel) {
        return Err("another forwarding adapter host is attached".to_owned());
    }
    *relay.grant.lock().expect("relay poisoned") = profiles;
    let outcome = run(&relay, socket).await;
    let why = match &outcome {
        Ok(()) => "the forwarding adapter host disconnected".to_owned(),
        Err(why) => why.clone(),
    };
    tracing::warn!("forwarding adapter host: {why}");
    relay.down(&why);
    relay.busy.store(false, Ordering::Release);
    outcome
}

/// Descriptor, then frames until the connection closes.
async fn run<S>(relay: &Arc<Relay>, socket: WebSocketStream<S>) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sink, mut stream) = socket.split();
    let first = tokio::time::timeout(DESCRIPTOR_TIMEOUT, next_json(&mut stream))
        .await
        .map_err(|_| "no descriptor within 10 seconds".to_owned())??;
    let descriptor = wire::descriptor(&first)?;
    tracing::info!(platforms = ?descriptor.platforms, "relay connector connected");
    *relay.descriptor.lock().expect("relay poisoned") = Some(descriptor.clone());
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    *relay.out.lock().expect("relay poisoned") = Some(tx);
    relay.connected.send_replace(true);
    if let Some(shared) = relay.shared.get().and_then(Weak::upgrade) {
        super::adapters::front(&shared, relay, &descriptor);
    }
    let writer = pmt_core::spawn::spawn(async move {
        while let Some(message) = rx.recv().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
    });
    let result = loop {
        match next_json(&mut stream).await {
            Ok(frame) => handle(relay, frame),
            Err(why) if why.is_empty() => break Ok(()),
            Err(why) => break Err(why),
        }
    };
    writer.abort();
    result
}

/// The next JSON frame; `Err("")` when the connection closed.
async fn next_json<S>(stream: &mut S) -> Result<Value, String>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match stream.next().await {
            None | Some(Ok(Message::Close(_))) => return Err(String::new()),
            Some(Ok(Message::Text(text))) => {
                return serde_json::from_str(&text).map_err(|e| format!("bad relay frame: {e}"));
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(format!("relay connection failed: {e}")),
        }
    }
}

fn handle(relay: &Arc<Relay>, frame: Value) {
    match frame["type"].as_str() {
        Some("result") => {
            let id = frame["id"].as_str().unwrap_or_default().to_owned();
            if let Some(tx) = relay.pending.lock().expect("relay poisoned").remove(&id) {
                let _ = tx.send(frame);
            }
        }
        Some("inbound") => {
            if let Some((platform, inbound)) = wire::inbound(&frame) {
                let relay = Arc::clone(relay);
                pmt_core::spawn::spawn(admit(relay, platform, inbound).in_current_span());
            } else {
                tracing::warn!("relay sent an inbound frame without sender, chat or kind");
            }
        }
        // A newer connector's frame this build does not know.
        _ => {}
    }
}

async fn admit(relay: Arc<Relay>, platform: String, inbound: super::adapter::Inbound) {
    let Some(shared) = relay.shared.get().and_then(Weak::upgrade) else {
        return;
    };
    let message_id = inbound.message_id.clone();
    let admission = if relay.addresses(inbound.profile.as_deref()) {
        super::admit::admit(&shared, &relay.options.default_profile, &platform, inbound).await
    } else {
        tracing::warn!(
            platform,
            profile = inbound.profile.as_deref().unwrap_or_default(),
            "the forwarding adapter host's device is not bound to this profile; dropped"
        );
        super::adapter::Admission::Dropped
    };
    let frame = json!({"type": "admission", "message_id": message_id, "admission": admission});
    if let Some(out) = relay.out.lock().expect("relay poisoned").as_ref() {
        let _ = out.send(Message::text(frame.to_string()));
    }
}

impl Relay {
    /// Whether an event naming `profile` may be admitted: always for a
    /// dialed connector, within the device's binding for an attached host.
    fn addresses(&self, profile: Option<&str>) -> bool {
        if !self.attached {
            return true;
        }
        let grant = self.grant.lock().expect("relay poisoned");
        let profile = profile.unwrap_or(&self.options.default_profile);
        grant.iter().any(|p| p == profile)
    }

    /// The bytes of an inbound attachment the connector holds.
    pub(crate) async fn media(
        &self,
        (profile, platform): (&str, &str),
        location: &str,
    ) -> Result<super::adapter::MediaStream, super::adapter::SendError> {
        use super::adapter::SendError;
        use base64::Engine as _;
        let result = self
            .request(|id| wire::media(id, (profile, platform), location))
            .await
            .map_err(|e| SendError::Network(e.describe().to_owned()))?;
        if result["ok"].as_bool() != Some(true) {
            return Err(wire::send_result(&result)
                .err()
                .unwrap_or(SendError::Unsupported));
        }
        let data = base64::engine::general_purpose::STANDARD
            .decode(result["data"].as_str().unwrap_or_default())
            .map_err(|e| SendError::Permanent(format!("bad media data: {e}")))?;
        let declared = u64::try_from(data.len()).ok();
        Ok(super::adapter::MediaStream {
            declared_len: declared,
            chunks: Box::pin(futures_util::stream::iter([Ok(data)])),
        })
    }

    /// Sends (or with `edit_of` edits) a message through the connector.
    pub(crate) async fn send(
        &self,
        (profile, platform): (&str, &str),
        message: &super::adapter::Outgoing,
        edit_of: Option<&str>,
    ) -> Result<Option<String>, super::adapter::SendError> {
        use super::adapter::SendError;
        if !self.is_connected() {
            return Err(SendError::Network("the relay is not connected".to_owned()));
        }
        // An attached host runs elsewhere: the files travel in the frame.
        let carried = if self.attached {
            Some(carry(&message.attachments).await?)
        } else {
            None
        };
        let frame_of = |id: &str| {
            let mut frame = wire::outgoing(id, (profile, platform), message, edit_of);
            if let Some(carried) = &carried {
                frame["attachments"] = Value::Array(carried.clone());
            }
            frame
        };
        match self.request(frame_of).await {
            Ok(result) => wire::send_result(&result),
            Err(Unanswered::NotSent) => Err(SendError::Network(
                Unanswered::NotSent.describe().to_owned(),
            )),
            // Transmitted but unanswered: the platform may have it.
            Err(other) => Err(SendError::Ambiguous(other.describe().to_owned())),
        }
    }
}

/// The files of `attachments` with their bytes, for a host that cannot
/// read this machine's paths.
async fn carry(
    attachments: &[super::adapter::OutboundAttachment],
) -> Result<Vec<Value>, super::adapter::SendError> {
    use super::adapter::SendError;
    use base64::Engine as _;
    let mut carried = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        let path = std::path::Path::new(&attachment.path);
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|e| SendError::Permanent(format!("cannot read {}: {e}", attachment.path)))?;
        if bytes.len() > MAX_CARRIED_BYTES {
            return Err(SendError::Permanent(format!(
                "{} is larger than the {} MiB a forwarded attachment may carry",
                attachment.path,
                MAX_CARRIED_BYTES / (1024 * 1024)
            )));
        }
        carried.push(json!({
            "name": path.file_name().map(|n| n.to_string_lossy()),
            "as_document": attachment.as_document,
            "as_voice": attachment.as_voice,
            "data": base64::engine::general_purpose::STANDARD.encode(bytes),
        }));
    }
    Ok(carried)
}
