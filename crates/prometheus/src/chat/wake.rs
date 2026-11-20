//! The wake word on the interactive surfaces (spec: voice-mode, Wake word
//! listener; Wake engines and false-trigger control; Single wake listener
//! ownership across surfaces; Wake word profile routing; Wake and voice
//! diagnostics).
//!
//! `/wake on` checks that a speech-to-text and a text-to-speech provider
//! resolve (a wake is answered aloud), selects the engine, takes the
//! machine's wake lock for this surface, saves `wake.enabled = true` and
//! starts a listening task: it opens the microphone (`wake.input_device`),
//! feeds the engine and, on the wake phrase, drops the microphone and
//! hands the detection to the surface ([`Woken`]), which records and
//! answers one request and then calls [`WakeClient::resume`]. Engines run
//! as workers of the managed Python runtime; under the debug-build audio
//! seam a stand-in engine hears the recorded fixtures.

use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError};

use pmt_config::ProfileScope;
use pmt_config::schema::{WakeConfig, WakeEngine as Kind};
use pmt_core::cancel::CancelToken;
use pmt_core::env::LaunchEnv;
use pmt_providers::resolve::SecretSource as _;
use pmt_voice::device::Microphone;
use pmt_voice::wake::command::{self, Command, Facts};
use pmt_voice::wake::engine::{self, Detection, PORCUPINE_SECRET, Support, Thresholds, WakeEngine};
use pmt_voice::wake::fixture::FixtureEngine;
use pmt_voice::wake::listener::{self, SharedHealth};
use pmt_voice::wake::owner::{self, Owner};
use pmt_voice::wake::worker::{self, Init, WorkerEngine};
use tokio::sync::mpsc;

use crate::chat::voice::VoiceClient;

/// Where a surface gets its detections.
pub type Woken = Arc<dyn Fn(Detection) + Send + Sync>;

/// A detection callback and the receiving end a surface's idle loop
/// takes detections from.
#[must_use]
pub fn channel() -> (Woken, mpsc::UnboundedReceiver<Detection>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let woken: Woken = Arc::new(move |detection| {
        let _ = tx.send(detection);
    });
    (woken, rx)
}

/// The engines this host can run.
pub struct HostSupport {
    uv: bool,
    porcupine_key: bool,
    fixture: bool,
}

impl HostSupport {
    /// What `scope` can run (`fixture`: the debug-build audio seam, whose
    /// stand-in engine stands for every engine).
    #[must_use]
    pub fn of(scope: &ProfileScope, fixture: bool) -> Self {
        Self {
            uv: pmt_update::runtimes::Store::open(scope.home())
                .installed("uv")
                .is_some(),
            porcupine_key: porcupine_key(scope).is_some(),
            fixture,
        }
    }
}

fn porcupine_key(scope: &ProfileScope) -> Option<String> {
    scope
        .secrets()
        .secret(PORCUPINE_SECRET)
        .ok()
        .flatten()
        .map(|key| key.expose().to_owned())
        .filter(|key| !key.is_empty())
}

impl Support for HostSupport {
    fn check(&self, kind: Kind) -> Result<(), String> {
        if kind == Kind::Porcupine && !self.porcupine_key {
            return Err(format!(
                "it needs the secret {PORCUPINE_SECRET} (prometheus secrets set {PORCUPINE_SECRET})"
            ));
        }
        if self.uv || self.fixture {
            Ok(())
        } else {
            Err(format!(
                "the engine is not installed: {}",
                worker::install_hint(kind)
            ))
        }
    }
}

/// Just the `[wake]` section of a profile's `config.toml`.
#[derive(serde::Deserialize)]
struct WakeSection {
    #[serde(default)]
    wake: WakeConfig,
}

/// The wake configuration of every other profile of the home (profile
/// routing enrolls the ones with wake enabled).
#[must_use]
pub fn other_profiles(scope: &ProfileScope) -> Vec<(String, WakeConfig)> {
    let Ok(home) = pmt_config::Home::open(scope.home()) else {
        return Vec::new();
    };
    home.profiles()
        .unwrap_or_default()
        .into_iter()
        .filter(|name| name.as_str() != scope.name().as_str())
        .filter_map(|name| {
            let dir = scope.home().join("profiles").join(name.as_str());
            let text = pmt_config::ConfigFile::of_profile_dir(&dir).raw().ok()??;
            let section: WakeSection = toml::from_str(&text).ok()?;
            Some((name.as_str().to_owned(), section.wake))
        })
        .collect()
}

/// Starts the `kind` engine for `scope`: the stand-in under the audio
/// seam (the detection it reports is read from the seam directory's
/// `wake` file: a phrase line and an optional profile line), else the
/// engine's worker on the managed Python runtime.
///
/// # Errors
///
/// The worker did not start or could not load its model.
pub async fn start_engine(
    scope: &ProfileScope,
    kind: Kind,
    test_audio: Option<&Path>,
) -> Result<Box<dyn WakeEngine>, String> {
    let config = &scope.config().wake;
    if let Some(dir) = test_audio {
        let scripted = std::fs::read_to_string(dir.join("wake")).unwrap_or_default();
        let mut lines = scripted.lines().map(str::trim).filter(|l| !l.is_empty());
        let detection = Detection {
            phrase: lines.next().unwrap_or(&config.phrase).to_lowercase(),
            profile: lines.next().map(str::to_owned),
        };
        return Ok(Box::new(FixtureEngine::new(detection)));
    }
    let (package, _) = worker::driver(kind).ok_or("no driver for this engine")?;
    let script = worker::script(kind).ok_or("no driver for this engine")?;
    let launcher = crate::chat::session::media::launcher(scope);
    let command = launcher.worker(kind.as_str(), package, &script)?;
    let thresholds = Thresholds::of(config.sensitivity);
    let (model, threshold) = match kind {
        Kind::Openwakeword => (config.openwakeword.model.clone(), thresholds.openwakeword),
        Kind::Porcupine => (config.porcupine.keyword.clone(), thresholds.porcupine),
        Kind::Sherpa | Kind::Auto => (String::new(), thresholds.sherpa),
    };
    let phrases = worker::enroll(
        (scope.name().as_str(), config),
        &if kind == Kind::Sherpa {
            other_profiles(scope)
        } else {
            Vec::new()
        },
    );
    let init = Init {
        engine: kind.as_str(),
        model,
        phrases,
        threshold,
        model_dir: config.sherpa.model_dir.clone().unwrap_or_else(|| {
            scope
                .dir()
                .join("cache")
                .join("speech-models")
                .join("wake")
                .display()
                .to_string()
        }),
        access_key: (kind == Kind::Porcupine)
            .then(|| porcupine_key(scope))
            .flatten(),
    };
    let engine =
        WorkerEngine::start(command, &init, &config.phrase, config.confirmation_frames).await?;
    Ok(Box::new(engine))
}

/// The input device `wake.input_device` names, as an index.
///
/// # Errors
///
/// No input device, or a name that matches none or several.
pub async fn input_device(
    mic: &dyn Microphone,
    config: &WakeConfig,
) -> Result<(usize, String), String> {
    let devices = mic.devices().await;
    if devices.is_empty() {
        return Err(pmt_voice::mode::NO_DEVICE.to_owned());
    }
    let index = match &config.input_device {
        Some(wanted) => command::pick_device(&devices, wanted)?,
        None => 0,
    };
    Ok((index, devices[index].clone()))
}

/// The listening task: microphone into engine, a detection out, then wait
/// for the surface to answer it.
struct Listening {
    mic: Arc<dyn Microphone>,
    device: usize,
    engine: Box<dyn WakeEngine>,
    health: SharedHealth,
    woken: Woken,
    resume: mpsc::UnboundedReceiver<()>,
    stop: CancelToken,
}

impl Listening {
    async fn run(mut self) {
        loop {
            let mut capture = match self.mic.open_at(self.device).await {
                Ok(capture) => capture,
                Err(why) => return self.failed(why),
            };
            let heard = tokio::select! {
                heard = listener::wait(capture.as_mut(), self.engine.as_mut(), &self.health) => heard,
                () = self.stop.cancelled() => return,
            };
            // The microphone is free for the request before anyone hears
            // of the wake.
            drop(capture);
            match heard {
                Ok(detection) => (self.woken)(detection),
                Err(why) => return self.failed(why),
            }
            tokio::select! {
                more = self.resume.recv() => if more.is_none() { return },
                () = self.stop.cancelled() => return,
            }
            self.engine.reset().await;
        }
    }

    fn failed(&self, why: String) {
        tracing::warn!(%why, "the wake listener stopped");
        self.health
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .error = Some(why);
    }
}

/// An armed listener.
struct Armed {
    owner: Owner,
    stop: CancelToken,
    resume: mpsc::UnboundedSender<()>,
}

/// The wake word of one interactive surface.
pub struct WakeClient {
    scope: ProfileScope,
    test_audio: Option<PathBuf>,
    surface: &'static str,
    armed: Option<Armed>,
    health: SharedHealth,
}

impl WakeClient {
    /// The wake word of `surface` (`cli`, `tui`), off.
    #[must_use]
    pub fn new(scope: &ProfileScope, launch: &LaunchEnv, surface: &'static str) -> Self {
        Self {
            scope: scope.clone(),
            test_audio: launch.test_audio_dir().cloned(),
            surface,
            armed: None,
            health: SharedHealth::default(),
        }
    }

    /// Whether this surface listens.
    #[must_use]
    pub fn armed(&self) -> bool {
        self.armed.is_some()
    }

    /// Runs `/wake <args>`; detections go to `woken`. The line to show.
    pub async fn command(&mut self, args: &str, voice: &mut VoiceClient, woken: &Woken) -> String {
        match Command::parse(args) {
            Ok(Command::On) => self.on(voice, woken, true).await,
            Ok(Command::Off) => self.off(true),
            Ok(Command::Status) => self.status(voice).await,
            Err(usage) => usage,
        }
    }

    /// Arms the listener at startup when `wake.enabled` is set; the
    /// refusal to show when it cannot.
    pub async fn start(&mut self, voice: &mut VoiceClient, woken: &Woken) -> Option<String> {
        if !self.scope.config().wake.enabled {
            return None;
        }
        Some(self.on(voice, woken, false).await)
    }

    /// `/wake on`; `save` writes `wake.enabled = true`.
    async fn on(&mut self, voice: &mut VoiceClient, woken: &Woken, save: bool) -> String {
        if self.armed.is_some() {
            return format!(
                "Wake word already armed: say \"{}\".",
                self.scope.config().wake.phrase
            );
        }
        match self.arm(voice, woken).await {
            Ok(kind) => {
                if save
                    && let Err(err) = self
                        .scope
                        .profile()
                        .config_file()
                        .set("wake.enabled", "true")
                {
                    return format!(
                        "Wake word armed, but wake.enabled was not saved: {}",
                        err.message()
                    );
                }
                format!(
                    "Wake word armed: say \"{}\" (engine {}).",
                    self.scope.config().wake.phrase,
                    kind.as_str()
                )
            }
            Err(why) => why,
        }
    }

    async fn arm(&mut self, voice: &mut VoiceClient, woken: &Woken) -> Result<Kind, String> {
        let config = (*self.scope.config().wake).clone();
        let facts = voice.probe().await;
        if let Some(refused) = command::providers_missing(&facts.stt, &facts.tts) {
            return Err(refused);
        }
        let support = HostSupport::of(&self.scope, self.test_audio.is_some());
        let kind = engine::select(&config, &support)?;
        let (audio, _) = voice.audio().await;
        let (device, _) = input_device(audio.mic.as_ref(), &config).await?;
        let me = Owner::this(self.surface, self.scope.name().as_str());
        owner::acquire(self.scope.home(), &me, config.surface).map_err(|r| r.message())?;
        let engine = match start_engine(&self.scope, kind, self.test_audio.as_deref()).await {
            Ok(engine) => engine,
            Err(why) => {
                owner::release(self.scope.home(), &me);
                return Err(format!("Wake word not armed: {why}"));
            }
        };
        let (resume, resumed) = mpsc::unbounded_channel();
        let stop = CancelToken::new();
        *self.health.lock().unwrap_or_else(PoisonError::into_inner) = listener::Health::default();
        let listening = Listening {
            mic: audio.mic,
            device,
            engine,
            health: Arc::clone(&self.health),
            woken: Arc::clone(woken),
            resume: resumed,
            stop: stop.clone(),
        };
        pmt_core::spawn::spawn(listening.run());
        self.armed = Some(Armed {
            owner: me,
            stop,
            resume,
        });
        Ok(kind)
    }

    /// `/wake off`; `save` writes `wake.enabled = false`.
    fn off(&mut self, save: bool) -> String {
        self.disarm();
        if save
            && let Err(err) = self
                .scope
                .profile()
                .config_file()
                .set("wake.enabled", "false")
        {
            return format!(
                "Wake word off, but wake.enabled was not saved: {}",
                err.message()
            );
        }
        "Wake word off.".to_owned()
    }

    /// Stops listening and gives the microphone up.
    pub fn disarm(&mut self) {
        if let Some(armed) = self.armed.take() {
            armed.stop.cancel();
            owner::release(self.scope.home(), &armed.owner);
        }
    }

    /// Listening resumes after a wake was answered.
    pub fn resume(&self) {
        if let Some(armed) = &self.armed {
            let _ = armed.resume.send(());
        }
    }

    /// `/wake status`.
    async fn status(&mut self, voice: &mut VoiceClient) -> String {
        let facts = self.facts(voice).await;
        command::status(&self.scope.config().wake, &facts)
    }

    /// What `/wake status` and `prometheus doctor` report.
    pub async fn facts(&mut self, voice: &mut VoiceClient) -> Facts {
        let config = (*self.scope.config().wake).clone();
        let support = HostSupport::of(&self.scope, self.test_audio.is_some());
        let engine = engine::select(&config, &support);
        let (audio, _) = voice.audio().await;
        let mut missing = Vec::new();
        if let Err(why) = &engine {
            missing.push(why.clone());
        }
        let device = input_device(audio.mic.as_ref(), &config)
            .await
            .map(|(_, name)| name);
        if device.is_err() {
            missing.push(format!(
                "a recorder for the microphone ({})",
                audio.mic.host_api()
            ));
        }
        Facts {
            armed: self.armed.is_some(),
            engine,
            owner: owner::current(self.scope.home()),
            me: std::process::id(),
            capture: command::capture(config.capture, false),
            device,
            host_api: audio.mic.host_api(),
            missing,
            health: self
                .health
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        }
    }

    /// The line a detection of another profile's phrase prints on a
    /// single-profile surface (it does not route).
    #[must_use]
    pub fn elsewhere(&self, detection: &Detection) -> Option<String> {
        let profile = detection.profile.as_deref()?;
        (profile != self.scope.name().as_str()).then(|| {
            format!(
                "\"{}\" wakes profile {profile}: run `prometheus -p {profile}`",
                detection.phrase
            )
        })
    }
}

impl Drop for WakeClient {
    fn drop(&mut self) {
        self.disarm();
    }
}
