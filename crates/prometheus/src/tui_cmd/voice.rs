//! Voice mode in the TUI (spec: voice-mode, Voice mode toggle and
//! commands; Push-to-talk recording and continuous loop; Spoken and typed
//! stop phrases). The conversation of [`pmt_voice`] runs as a task beside
//! the screen's event loop: it asks the screen (through [`Done::Voice`])
//! to submit each transcript, to handle each typed line and to show the
//! level meter, and the screen hands it the record key and typed lines
//! while it listens. Recording and speech happen on this machine; the
//! daemon runs the turns, and the voice turn's reply is spoken while it
//! generates, from its `assistant.delta` events.

use std::sync::Arc;

use pmt_core::cancel::CancelToken;
use pmt_rpc::contracts::{InterruptParams, SessionInterrupt, TurnFinished};
use pmt_voice::converse::{self, Ended, Host, Interjection};
use pmt_voice::speaker::Mouth;
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot};

use super::screen::{Done, Tui};
use crate::chat::voice::VoiceClient;
use crate::chat::wake::{WakeClient, Woken};

/// The status-line row the level meter uses.
const METER_ROW: &str = "voice";

/// What the conversation task asks of the screen.
pub(super) enum Ask {
    /// `/voice` answered: the line to show and whether voice mode is on.
    Said(String, bool),
    /// The user spoke over the voice turn's reply: interrupt the turn.
    Cut,
    /// Submit a transcript; answer once its turn has finished.
    Turn {
        text: String,
        mouth: Option<Arc<Mouth>>,
        done: oneshot::Sender<()>,
    },
    /// Handle a typed line as typed input; answer whether the
    /// conversation goes on.
    Typed {
        line: String,
        done: oneshot::Sender<bool>,
    },
    /// The meter line (input level and live transcript) while recording.
    Level(String),
    /// The conversation ended.
    Ended(Ended),
    /// `/wake` answered.
    WakeSaid(String),
    /// The wake listener heard its phrase.
    Woken(pmt_voice::wake::engine::Detection),
    /// A wake's request was answered (or failed): listening resumes.
    WakeAnswered(Result<(), String>),
}

/// A conversation running beside the screen.
pub(super) struct Talk {
    interject: mpsc::UnboundedSender<Interjection>,
    /// The voice turn the screen waits on.
    turn: Option<VoiceTurn>,
}

/// A submitted voice turn.
struct VoiceTurn {
    mouth: Option<Arc<Mouth>>,
    /// Answered when the turn finished (or will not run).
    done: oneshot::Sender<()>,
    /// The turn's id once `prompt.submit` admitted it.
    id: Option<String>,
    /// Answer pieces that arrived before the admission did (events and
    /// the submission's answer travel separately): `(turn id, text)`.
    early: Vec<(String, String)>,
}

/// The screen's side of voice mode.
pub(super) struct Voice {
    pub(super) client: Arc<Mutex<VoiceClient>>,
    /// Voice mode is on (the record key talks).
    pub(super) on: bool,
    pub(super) talk: Option<Talk>,
    /// `/wake` and this terminal's wake listener.
    pub(super) wake: Arc<Mutex<WakeClient>>,
    /// Where the listener hands its detections (to the screen).
    woken: Woken,
    /// A wake waits for its new session before recording the request.
    wake_pending: bool,
}

impl Voice {
    pub(super) fn new(
        client: VoiceClient,
        wake: WakeClient,
        done: &mpsc::UnboundedSender<Done>,
    ) -> Self {
        let done = done.clone();
        let woken: Woken = Arc::new(move |detection| {
            let _ = done.send(Done::Voice(Ask::Woken(detection)));
        });
        Self {
            client: Arc::new(Mutex::new(client)),
            on: false,
            talk: None,
            wake: Arc::new(Mutex::new(wake)),
            woken,
            wake_pending: false,
        }
    }
}

/// The conversation's surface: channels to the screen.
struct Channel {
    asks: mpsc::UnboundedSender<Done>,
    interjections: mpsc::UnboundedReceiver<Interjection>,
}

impl Host for Channel {
    async fn turn(&mut self, text: String, mouth: Option<Arc<Mouth>>, cut: CancelToken) {
        let (done, finished) = oneshot::channel();
        if self
            .asks
            .send(Done::Voice(Ask::Turn { text, mouth, done }))
            .is_err()
        {
            return;
        }
        tokio::pin!(finished);
        tokio::select! {
            _ = &mut finished => return,
            () = cut.cancelled() => {}
        }
        if self.asks.send(Done::Voice(Ask::Cut)).is_ok() {
            let _ = finished.await;
        }
    }

    async fn interjection(&mut self) -> Interjection {
        self.interjections
            .recv()
            .await
            .unwrap_or(Interjection::RecordKey)
    }

    async fn typed(&mut self, line: String) -> bool {
        let (done, answer) = oneshot::channel();
        if self
            .asks
            .send(Done::Voice(Ask::Typed { line, done }))
            .is_err()
        {
            return false;
        }
        answer.await.unwrap_or(false)
    }

    fn heard(&mut self, _transcript: &str) {
        // The submitted transcript shows as the user's message.
    }
}

impl Tui<'_> {
    /// `/voice [on|off|tts|status]`, answered in the background (it probes
    /// the input device and the providers).
    pub(super) fn voice_command(&self, args: &str) {
        let client = Arc::clone(&self.voice.client);
        let args = args.to_owned();
        self.background(async move {
            let mut client = client.lock().await;
            let said = client.command(&args).await;
            Done::Voice(Ask::Said(said, client.mode.on))
        });
    }

    /// `/wake [on|off|status]`, answered in the background.
    pub(super) fn wake_command(&self, args: &str) {
        let (client, wake) = (Arc::clone(&self.voice.client), Arc::clone(&self.voice.wake));
        let woken = Arc::clone(&self.voice.woken);
        let args = args.to_owned();
        self.background(async move {
            let mut client = client.lock().await;
            let said = wake.lock().await.command(&args, &mut client, &woken).await;
            Done::Voice(Ask::WakeSaid(said))
        });
    }

    /// Arms the wake listener at startup when `wake.enabled` is set.
    pub(super) fn arm_wake(&self) {
        let (client, wake) = (Arc::clone(&self.voice.client), Arc::clone(&self.voice.wake));
        let woken = Arc::clone(&self.voice.woken);
        let done = self.done.clone();
        pmt_core::spawn::spawn(async move {
            let mut client = client.lock().await;
            if let Some(said) = wake.lock().await.start(&mut client, &woken).await {
                let _ = done.send(Done::Voice(Ask::WakeSaid(said)));
            }
        });
    }

    /// The wake phrase was heard: a new session first (unless
    /// `wake.start_new_session = false`), then one request answered
    /// aloud.
    fn woken(&mut self, detection: &pmt_voice::wake::engine::Detection) {
        let elsewhere = self
            .voice
            .wake
            .try_lock()
            .ok()
            .and_then(|wake| wake.elsewhere(detection));
        if let Some(elsewhere) = elsewhere {
            self.notice(elsewhere);
            let wake = Arc::clone(&self.voice.wake);
            pmt_core::spawn::spawn(async move { wake.lock().await.resume() });
            return;
        }
        if self.voice.talk.is_some() {
            // A voice conversation holds the microphone: the wake waits.
            let wake = Arc::clone(&self.voice.wake);
            pmt_core::spawn::spawn(async move { wake.lock().await.resume() });
            return;
        }
        self.notice(format!("Heard \"{}\".", detection.phrase));
        if self.open.scope.config().wake.start_new_session {
            self.voice.wake_pending = true;
            self.start_session(None, None);
        } else {
            self.answer_wake();
        }
    }

    /// The new session of a wake is shown: record the request now.
    pub(super) fn wake_session_ready(&mut self) {
        if std::mem::take(&mut self.voice.wake_pending) {
            self.answer_wake();
        }
    }

    /// Records the request after a wake and answers it aloud.
    fn answer_wake(&mut self) {
        let (interject, interjections) = mpsc::unbounded_channel();
        self.voice.talk = Some(Talk {
            interject,
            turn: None,
        });
        let asks = self.done.clone();
        let client = Arc::clone(&self.voice.client);
        let key = pmt_voice::mode::record_key(&self.open.scope.config().voice);
        let session = self.link.session.clone();
        let strings = pmt_config::language::strings(self.open.scope.dir(), pmt_i18n::Surface::Core);
        pmt_core::spawn::spawn(async move {
            let lines = asks.clone();
            let show: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |line| {
                let _ = lines.send(Done::Voice(Ask::Level(line)));
            });
            let (meter, partial) = crate::chat::voice::status_feed(strings, key, show);
            let kit = client
                .lock()
                .await
                .kit(meter, partial, &session, true)
                .await;
            let answered = match kit {
                Ok(kit) => {
                    let mut host = Channel {
                        asks: asks.clone(),
                        interjections,
                    };
                    pmt_voice::wake::listener::answer(&mut host, &kit)
                        .await
                        .map(|_| ())
                }
                Err(why) => Err(why),
            };
            let _ = asks.send(Done::Voice(Ask::WakeAnswered(answered)));
        });
    }

    /// The record key: starts a conversation, or stops the one running.
    pub(super) fn record_key(&mut self) {
        if let Some(talk) = &self.voice.talk {
            let _ = talk.interject.send(Interjection::RecordKey);
            return;
        }
        let (interject, interjections) = mpsc::unbounded_channel();
        self.voice.talk = Some(Talk {
            interject,
            turn: None,
        });
        let asks = self.done.clone();
        let client = Arc::clone(&self.voice.client);
        let key = pmt_voice::mode::record_key(&self.open.scope.config().voice);
        let session = self.link.session.clone();
        self.notice(pmt_i18n::t!(
            self.strings(),
            "tui.listening",
            0 = key.clone()
        ));
        let strings = pmt_config::language::strings(self.open.scope.dir(), pmt_i18n::Surface::Core);
        pmt_core::spawn::spawn(async move {
            let lines = asks.clone();
            let show: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |line| {
                let _ = lines.send(Done::Voice(Ask::Level(line)));
            });
            let (meter, partial) = crate::chat::voice::status_feed(strings, key, show);
            let kit = {
                let mut client = client.lock().await;
                let spoken = client.mode.spoken;
                client.kit(meter, partial, &session, spoken).await
            };
            let ended = match kit {
                Ok(kit) => {
                    let mut host = Channel {
                        asks: asks.clone(),
                        interjections,
                    };
                    converse::converse(&mut host, &kit).await
                }
                Err(why) => Ended::Failed(why),
            };
            let _ = asks.send(Done::Voice(Ask::Ended(ended)));
        });
    }

    /// A line submitted while a conversation listens goes to it (a stop
    /// phrase ends it); `false` when no conversation runs.
    pub(super) fn voice_line(&mut self, line: &str) -> bool {
        let Some(talk) = &self.voice.talk else {
            return false;
        };
        if line.trim().is_empty() {
            return true;
        }
        talk.interject
            .send(Interjection::Line(line.to_owned()))
            .is_ok()
    }

    /// What the conversation task asked.
    pub(super) fn on_voice(&mut self, ask: Ask) {
        match ask {
            Ask::Said(said, on) => {
                self.voice.on = on;
                self.notice(said);
            }
            Ask::Turn { text, mouth, done } => {
                if let Some(talk) = &mut self.voice.talk {
                    talk.turn = Some(VoiceTurn {
                        mouth,
                        done,
                        id: None,
                        early: Vec::new(),
                    });
                }
                self.submit_as(text, Vec::new(), true);
            }
            Ask::Cut => {
                let params = InterruptParams {
                    session_id: self.link.session.clone(),
                    turn_id: None,
                };
                let client = self.link.client.clone();
                pmt_core::spawn::spawn(async move {
                    let _ = client.call_typed::<SessionInterrupt>(&params).await;
                });
            }
            Ask::Typed { line, done } => {
                self.run_line(&line);
                let _ = done.send(self.voice.on);
            }
            Ask::Level(line) => {
                self.app
                    .status
                    .automation
                    .insert(METER_ROW.to_owned(), line);
            }
            Ask::Ended(ended) => {
                self.app.status.automation.remove(METER_ROW);
                self.voice.talk = None;
                self.notice(ended.message());
            }
            Ask::WakeSaid(said) => self.notice(said),
            Ask::Woken(detection) => self.woken(&detection),
            Ask::WakeAnswered(answered) => {
                self.app.status.automation.remove(METER_ROW);
                self.voice.talk = None;
                if let Err(why) = answered {
                    self.notice(why);
                }
                let wake = Arc::clone(&self.voice.wake);
                pmt_core::spawn::spawn(async move { wake.lock().await.resume() });
            }
        }
    }

    /// The voice turn was admitted as turn `id`, or will not run (`None`).
    pub(super) fn voice_admitted(&mut self, id: Option<&str>) {
        let Some(talk) = &mut self.voice.talk else {
            return;
        };
        let Some(id) = id else {
            if let Some(turn) = talk.turn.take() {
                let _ = turn.done.send(());
            }
            return;
        };
        let Some(turn) = &mut talk.turn else {
            return;
        };
        turn.id = Some(id.to_owned());
        for (of, text) in std::mem::take(&mut turn.early) {
            if of == id
                && let Some(mouth) = &turn.mouth
            {
                mouth.text(&text);
            }
        }
    }

    /// A piece of a turn's answer (an `assistant.delta` of the shown
    /// session): the voice turn's is spoken as it arrives.
    pub(super) fn voice_delta(&mut self, data: &Value) {
        let Some(turn) = self.voice.talk.as_mut().and_then(|t| t.turn.as_mut()) else {
            return;
        };
        let Some(mouth) = &turn.mouth else {
            return;
        };
        let of = data["turn_id"].as_str().unwrap_or_default();
        let text = data["payload"]["text"].as_str().unwrap_or_default();
        match &turn.id {
            Some(id) if id == of => mouth.text(text),
            Some(_) => {}
            None => turn.early.push((of.to_owned(), text.to_owned())),
        }
    }

    /// A turn of the shown session finished: when it is the voice turn,
    /// the conversation goes on (the mouth says the rest and waits).
    pub(super) fn voice_turn_finished(&mut self, finished: &TurnFinished) {
        let Some(talk) = &mut self.voice.talk else {
            return;
        };
        if talk
            .turn
            .as_ref()
            .is_some_and(|turn| turn.id.as_deref() == Some(finished.turn_id.as_str()))
            && let Some(turn) = talk.turn.take()
        {
            let _ = turn.done.send(());
        }
    }
}
