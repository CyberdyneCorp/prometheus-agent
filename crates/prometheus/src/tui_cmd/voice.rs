//! Voice mode in the TUI (spec: voice-mode, Voice mode toggle and
//! commands; Push-to-talk recording and continuous loop; Spoken and typed
//! stop phrases). The conversation of [`pmt_voice`] runs as a task beside
//! the screen's event loop: it asks the screen (through [`Done::Voice`])
//! to submit each transcript, to handle each typed line and to show the
//! level meter, and the screen hands it the record key and typed lines
//! while it listens. Recording and speech happen on this machine; the
//! daemon runs the turns and sends whole messages, so a reply is spoken
//! once its message arrives.

use std::sync::Arc;

use pmt_rpc::contracts::TurnFinished;
use pmt_voice::converse::{self, Ended, Host, Interjection};
use pmt_voice::record::Level;
use pmt_voice::speaker::Mouth;
use tokio::sync::{Mutex, mpsc, oneshot};

use super::screen::{Done, Tui};
use crate::chat::voice::VoiceClient;

/// The status-line row the level meter uses.
const METER_ROW: &str = "voice";

/// What the conversation task asks of the screen.
pub(super) enum Ask {
    /// `/voice` answered: the line to show and whether voice mode is on.
    Said(String, bool),
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
    /// The input level while recording.
    Level(Level),
    /// The conversation ended.
    Ended(Ended),
}

/// A conversation running beside the screen.
pub(super) struct Talk {
    interject: mpsc::UnboundedSender<Interjection>,
    /// The voice turn the screen waits on: its mouth and its answer.
    turn: Option<(Option<Arc<Mouth>>, oneshot::Sender<()>)>,
    /// The voice turn was submitted and admitted (its end is the next
    /// `turn.finished` of the shown session).
    submitted: bool,
}

/// The screen's side of voice mode.
pub(super) struct Voice {
    pub(super) client: Arc<Mutex<VoiceClient>>,
    /// Voice mode is on (the record key talks).
    pub(super) on: bool,
    pub(super) talk: Option<Talk>,
}

impl Voice {
    pub(super) fn new(client: VoiceClient) -> Self {
        Self {
            client: Arc::new(Mutex::new(client)),
            on: false,
            talk: None,
        }
    }
}

/// The conversation's surface: channels to the screen.
struct Channel {
    asks: mpsc::UnboundedSender<Done>,
    interjections: mpsc::UnboundedReceiver<Interjection>,
}

impl Host for Channel {
    async fn turn(&mut self, text: String, mouth: Option<Arc<Mouth>>) {
        let (done, finished) = oneshot::channel();
        if self
            .asks
            .send(Done::Voice(Ask::Turn { text, mouth, done }))
            .is_ok()
        {
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
            submitted: false,
        });
        let asks = self.done.clone();
        let client = Arc::clone(&self.voice.client);
        let key = pmt_voice::mode::record_key(&self.open.scope.config().voice);
        self.notice(format!("listening… ({key} stops)"));
        tokio::spawn(async move {
            let levels = asks.clone();
            let meter: pmt_voice::record::Meter = Arc::new(move |level| {
                let _ = levels.send(Done::Voice(Ask::Level(level)));
            });
            let kit = client.lock().await.kit(meter).await;
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
                    talk.turn = Some((mouth, done));
                    talk.submitted = false;
                }
                self.submit(text, Vec::new());
            }
            Ask::Typed { line, done } => {
                self.run_line(&line);
                let _ = done.send(self.voice.on);
            }
            Ask::Level(level) => {
                let key = pmt_voice::mode::record_key(&self.open.scope.config().voice);
                self.app.status.automation.insert(
                    METER_ROW.to_owned(),
                    crate::chat::voice::meter_line(level, &key),
                );
            }
            Ask::Ended(ended) => {
                self.app.status.automation.remove(METER_ROW);
                self.voice.talk = None;
                self.notice(ended.message());
            }
        }
    }

    /// The voice turn was admitted (`true`), or will not run.
    pub(super) fn voice_admitted(&mut self, runs: bool) {
        let Some(talk) = &mut self.voice.talk else {
            return;
        };
        if runs {
            talk.submitted = true;
        } else if let Some((_, done)) = talk.turn.take() {
            let _ = done.send(());
        }
    }

    /// A turn of the shown session finished: the voice turn's answer is
    /// spoken and the conversation goes on.
    pub(super) fn voice_turn_finished(&mut self, finished: &TurnFinished) {
        let Some(talk) = &mut self.voice.talk else {
            return;
        };
        if !talk.submitted {
            return;
        }
        if let Some((mouth, done)) = talk.turn.take() {
            if let (Some(mouth), Some(answer)) = (mouth, &finished.final_response) {
                mouth.text(answer);
            }
            let _ = done.send(());
        }
        talk.submitted = false;
    }
}
