//! The daemon's wake listeners: the desktop's ear (spec: voice-mode, Wake
//! capture location for remote clients; Single wake listener ownership
//! across surfaces; Wake and voice diagnostics).
//!
//! `wake.arm` runs the profile's engine in the daemon, on the daemon's
//! microphone (`capture = "local"`) or on the 16 kHz frames the arming
//! client sends with `wake.feed` (`client`, or `auto` with
//! `client_capture`). The listener takes the machine's wake lock for the
//! desktop surface and sends `wake.detected` to the arming connection;
//! the desktop then records the request on its own microphone. Wake audio
//! never leaves the daemon: frames go to the engine only. The listener
//! ends, and frees the lock, when it is disarmed or its connection
//! closes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use pmt_config::ProfileScope;
use pmt_config::schema::WakeCapture;
use pmt_core::cancel::CancelToken;
use pmt_core::error::{Error, codes};
use pmt_gateway::voice::{Caller, VoiceCall};
use pmt_rpc::contracts::{WakeArmParams, WakeArmed, WakeDetected, WakeFeedParams, WakeState};
use pmt_voice::device::{BoxFuture, Capture};
use pmt_voice::wake::command::{self, Facts};
use pmt_voice::wake::engine::{self, WakeEngine};
use pmt_voice::wake::listener::{self, SharedHealth};
use pmt_voice::wake::owner::{self, Owner};
use serde_json::Value;
use tokio::sync::mpsc;

use super::DaemonHost;
use crate::chat::wake::{HostSupport, start_engine};

/// Frames a client may queue ahead of the engine (10 s of 100 ms
/// chunks); more are refused rather than buffered.
const QUEUED_FRAMES: usize = 100;

/// The armed listeners, by profile.
#[derive(Default)]
pub(super) struct Listeners(Mutex<HashMap<String, Armed>>);

struct Armed {
    owner: Owner,
    stop: CancelToken,
    /// Client capture: where `wake.feed` frames go.
    frames: Option<mpsc::Sender<Vec<i16>>>,
    health: SharedHealth,
    capture: WakeCapture,
}

impl Listeners {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Armed>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(&codes::RPC_INVALID_PARAMS, message)
}

fn to_value<T: serde::Serialize>(value: &T) -> Result<Value, Error> {
    serde_json::to_value(value).map_err(|e| Error::new(&codes::RPC_INTERNAL, e.to_string()))
}

/// Frames from the client's microphone; ends when the client
/// disconnects or the listener is disarmed.
struct ClientFrames {
    rx: mpsc::Receiver<Vec<i16>>,
    client: Caller,
}

impl Capture for ClientFrames {
    fn read(&mut self) -> BoxFuture<'_, Option<Vec<i16>>> {
        Box::pin(async move {
            loop {
                match tokio::time::timeout(Duration::from_secs(1), self.rx.recv()).await {
                    Ok(frame) => return frame,
                    Err(_) if !self.client.connected() => return None,
                    Err(_) => {}
                }
            }
        })
    }
}

/// The local microphone, given up when the arming client disconnects.
struct LocalFrames {
    capture: Box<dyn Capture>,
    client: Caller,
}

impl Capture for LocalFrames {
    fn read(&mut self) -> BoxFuture<'_, Option<Vec<i16>>> {
        Box::pin(async move {
            if !self.client.connected() {
                return None;
            }
            self.capture.read().await
        })
    }
}

impl DaemonHost {
    /// One voice call (`wake.*`, `voice.client_config`, `voice.transcribe`)
    /// for `profile`.
    pub(super) async fn voice_call(
        &self,
        profile: &str,
        request: VoiceCall,
    ) -> Result<Value, Error> {
        let scope = self.profile(profile)?.scope();
        match request.method {
            "wake.arm" => {
                let params: WakeArmParams =
                    serde_json::from_value(request.params).map_err(|e| invalid(e.to_string()))?;
                Box::pin(self.wake_arm(&scope, params, request.client)).await
            }
            "wake.feed" => {
                let params: WakeFeedParams =
                    serde_json::from_value(request.params).map_err(|e| invalid(e.to_string()))?;
                self.wake_feed(profile, &params)
            }
            "wake.disarm" => {
                self.wake_disarm(&scope);
                to_value(&WakeState::default())
            }
            "wake.status" => to_value(&self.wake_status(&scope).await),
            "voice.client_config" => to_value(&super::voice_client::config(&scope)),
            "voice.transcribe" => {
                let params =
                    serde_json::from_value(request.params).map_err(|e| invalid(e.to_string()))?;
                Box::pin(super::voice_client::transcribe(&scope, params)).await
            }
            other => Err(invalid(format!("unknown voice method {other}"))),
        }
    }

    async fn wake_arm(
        &self,
        scope: &ProfileScope,
        params: WakeArmParams,
        client: Caller,
    ) -> Result<Value, Error> {
        let profile = scope.name().as_str().to_owned();
        self.wake_disarm(scope);
        let config = (*scope.config().wake).clone();
        let test_audio = self.launch.test_audio_dir().cloned();
        let refuse = |why: String| Error::new(&codes::RPC_NOT_SERVED, why);
        let media = crate::chat::voice::media(scope, "gui", scope.dir());
        let audio = crate::chat::voice::audio(scope, test_audio.as_deref()).await;
        let facts = crate::chat::voice::facts(media.as_deref(), audio.mic.as_ref()).await;
        if let Some(refused) = command::providers_missing(&facts.stt, &facts.tts) {
            return Err(refuse(refused));
        }
        let kind = engine::select(&config, &HostSupport::of(scope, test_audio.is_some()))
            .map_err(refuse)?;
        let capture = command::capture(config.capture, params.client_capture);
        let me = Owner::this("gui", &profile);
        owner::acquire(scope.home(), &me, config.surface).map_err(|r| refuse(r.message()))?;
        let armed = Box::pin(self.start_listening(scope, kind, capture, &client)).await;
        let (source, frames, engine) = match armed {
            Ok(parts) => parts,
            Err(why) => {
                owner::release(scope.home(), &me);
                return Err(refuse(format!("Wake word not armed: {why}")));
            }
        };
        let health = SharedHealth::default();
        let stop = CancelToken::new();
        let task = Listening {
            profile: profile.clone(),
            home: scope.home().to_path_buf(),
            source,
            engine,
            health: Arc::clone(&health),
            client,
            stop: stop.clone(),
            owner: me.clone(),
        };
        pmt_core::spawn::spawn(task.run());
        self.wake.lock().insert(
            profile,
            Armed {
                owner: me,
                stop,
                frames,
                health,
                capture,
            },
        );
        to_value(&WakeArmed {
            engine: kind.as_str().to_owned(),
            phrase: config.phrase,
            capture: capture.as_str().to_owned(),
        })
    }

    /// The listener's audio source and engine.
    async fn start_listening(
        &self,
        scope: &ProfileScope,
        kind: pmt_config::schema::WakeEngine,
        capture: WakeCapture,
        client: &Caller,
    ) -> Result<
        (
            Box<dyn Capture>,
            Option<mpsc::Sender<Vec<i16>>>,
            Box<dyn WakeEngine>,
        ),
        String,
    > {
        let test_audio = self.launch.test_audio_dir().cloned();
        let (source, frames): (Box<dyn Capture>, _) = if capture == WakeCapture::Client {
            let (tx, rx) = mpsc::channel(QUEUED_FRAMES);
            let frames = ClientFrames {
                rx,
                client: client.clone(),
            };
            (Box::new(frames), Some(tx))
        } else {
            let audio = crate::chat::voice::audio(scope, test_audio.as_deref()).await;
            let (device, _) =
                crate::chat::wake::input_device(audio.mic.as_ref(), &scope.config().wake).await?;
            let capture = audio.mic.open_at(device).await?;
            (
                Box::new(LocalFrames {
                    capture,
                    client: client.clone(),
                }),
                None,
            )
        };
        let engine = start_engine(scope, kind, test_audio.as_deref()).await?;
        Ok((source, frames, engine))
    }

    fn wake_feed(&self, profile: &str, params: &WakeFeedParams) -> Result<Value, Error> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(params.pcm.as_bytes())
            .map_err(|e| invalid(format!("pcm is not base64: {e}")))?;
        let samples = pmt_voice::pcm::samples_of(&bytes);
        let listeners = self.wake.lock();
        let Some(armed) = listeners.get(profile) else {
            return Err(Error::new(
                &codes::RPC_NOT_SERVED,
                "the wake listener is not armed",
            ));
        };
        let Some(frames) = &armed.frames else {
            return Err(invalid(
                "the wake listener captures locally; it takes no frames",
            ));
        };
        frames.try_send(samples).map_err(|_| {
            Error::new(
                &codes::RPC_NOT_SERVED,
                "the wake listener is behind or stopped",
            )
        })?;
        to_value(&WakeState {
            armed: true,
            ..WakeState::default()
        })
    }

    fn wake_disarm(&self, scope: &ProfileScope) {
        if let Some(armed) = self.wake.lock().remove(scope.name().as_str()) {
            armed.stop.cancel();
            owner::release(scope.home(), &armed.owner);
        }
    }

    async fn wake_status(&self, scope: &ProfileScope) -> WakeState {
        let config = (*scope.config().wake).clone();
        let test_audio = self.launch.test_audio_dir().cloned();
        let support = HostSupport::of(scope, test_audio.is_some());
        let engine = engine::select(&config, &support);
        let (armed, health, capture) = match self
            .wake
            .lock()
            .get(scope.name().as_str())
            .filter(|armed| !armed.stop.is_cancelled())
        {
            Some(armed) => (
                true,
                armed
                    .health
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone(),
                armed.capture,
            ),
            None => (
                false,
                listener::Health::default(),
                command::capture(config.capture, false),
            ),
        };
        let audio = crate::chat::voice::audio(scope, test_audio.as_deref()).await;
        let device = if capture == WakeCapture::Client {
            Ok("the client's microphone".to_owned())
        } else {
            crate::chat::wake::input_device(audio.mic.as_ref(), &config)
                .await
                .map(|(_, name)| name)
        };
        let mut missing: Vec<String> = engine.as_ref().err().cloned().into_iter().collect();
        if device.is_err() {
            missing.push(format!(
                "a recorder for the microphone ({})",
                audio.mic.host_api()
            ));
        }
        let facts = Facts {
            armed,
            engine,
            owner: owner::current(scope.home()),
            me: std::process::id(),
            capture,
            device,
            host_api: audio.mic.host_api(),
            missing,
            health: health.clone(),
        };
        WakeState {
            armed,
            report: command::status(&config, &facts),
            error: health.error,
        }
    }
}

/// One daemon listener's loop.
struct Listening {
    profile: String,
    home: std::path::PathBuf,
    source: Box<dyn Capture>,
    engine: Box<dyn WakeEngine>,
    health: SharedHealth,
    client: Caller,
    stop: CancelToken,
    owner: Owner,
}

impl Listening {
    async fn run(mut self) {
        loop {
            let heard = tokio::select! {
                heard = listener::wait(self.source.as_mut(), self.engine.as_mut(), &self.health) => heard,
                () = self.stop.cancelled() => break,
            };
            match heard {
                Ok(detection) => {
                    self.client.detected(&WakeDetected {
                        profile: detection.profile.unwrap_or_else(|| self.profile.clone()),
                        phrase: detection.phrase,
                    });
                    self.engine.reset().await;
                }
                Err(why) => {
                    if self.client.connected() {
                        tracing::warn!(profile = self.profile, %why, "the wake listener stopped");
                        self.health
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .error = Some(why);
                    }
                    break;
                }
            }
        }
        // A listener that ended on its own (its client left) reads as
        // disarmed.
        self.stop.cancel();
        owner::release(&self.home, &self.owner);
    }
}

impl std::fmt::Debug for Listeners {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listeners").finish_non_exhaustive()
    }
}
