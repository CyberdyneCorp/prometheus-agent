//! Voice mode in the line interface (spec: voice-mode, Voice mode toggle
//! and commands; Push-to-talk recording and continuous loop; Spoken and
//! typed stop phrases). `/voice` arms the record key; pressing it at the
//! idle prompt starts the continuous conversation of [`pmt_voice`], whose
//! transcripts run as ordinary user turns and whose replies are spoken
//! sentence by sentence when spoken replies are on. The level meter is
//! the bottom status line while a recording runs.

use std::sync::Arc;

use pmt_core::cancel::CancelToken;
use pmt_voice::converse::{self, Host, Interjection};
use pmt_voice::speaker::Mouth;

use super::input::Event;
use super::{Flow, Repl};
use crate::chat::theme::Role;

/// A turn's text deltas, spoken.
pub(super) struct Spoken(pub(super) Arc<Mouth>);

impl pmt_agent::turn::DeltaSink for Spoken {
    fn text(&self, text: &str) {
        self.0.text(text);
    }
}

impl Repl {
    /// `/voice [on|off|tts|status]`.
    pub(super) async fn voice_command(&mut self, args: &str) {
        let said = self.voice.command(args).await;
        self.input.set_voice(self.voice.mode.on);
        self.notice(&said);
    }

    /// The record key at the idle prompt: a voice conversation until it
    /// ends.
    pub(super) async fn talk(&mut self) -> Flow {
        let status = self.input.status_line();
        let quiet = self.display.quiet;
        let key = pmt_voice::mode::record_key(&self.factory.scope.config().voice);
        let show: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |line| {
            if !quiet {
                status.show(Some(line));
            }
        });
        let (meter, partial) = crate::chat::voice::status_feed(self.strings(), key, show);
        let session = self.live().id.to_string();
        let spoken = self.voice.mode.spoken;
        let kit = match self.voice.kit(meter, partial, &session, spoken).await {
            Ok(kit) => kit,
            Err(why) => {
                self.notice(&why);
                return Flow::Continue;
            }
        };
        let mut host = Terminal {
            repl: self,
            exit: false,
        };
        let ended = converse::converse(&mut host, &kit).await;
        let exit = host.exit;
        self.input.status_line().show(None);
        self.notice(&ended.message());
        if exit { Flow::Exit } else { Flow::Continue }
    }
}

impl Repl {
    /// `/wake [on|off|status]`.
    pub(super) async fn wake_command(&mut self, args: &str) {
        let woken = Arc::clone(&self.woken.0);
        let said = self.wake.command(args, &mut self.voice, &woken).await;
        self.notice(&said);
    }

    /// Arms the wake listener at startup when `wake.enabled` is set.
    pub(super) async fn arm_wake(&mut self) {
        let woken = Arc::clone(&self.woken.0);
        if let Some(said) = self.wake.start(&mut self.voice, &woken).await {
            self.notice(&said);
        }
    }

    /// The wake phrase was heard at the idle prompt: a new session (unless
    /// `wake.start_new_session = false`), one spoken request answered
    /// aloud, then listening resumes.
    pub(super) async fn woken(&mut self, detection: pmt_voice::wake::engine::Detection) -> Flow {
        if let Some(elsewhere) = self.wake.elsewhere(&detection) {
            self.notice(&elsewhere);
            self.wake.resume();
            return Flow::Continue;
        }
        self.notice(&format!("Heard \"{}\".", detection.phrase));
        if self.factory.scope.config().wake.start_new_session {
            match Box::pin(self.new_session(None, None)).await {
                Ok(opened) => self.notice(&opened),
                Err(err) => {
                    self.error(&err);
                    self.wake.resume();
                    return Flow::Continue;
                }
            }
        }
        let status = self.input.status_line();
        let quiet = self.display.quiet;
        let key = pmt_voice::mode::record_key(&self.factory.scope.config().voice);
        let show: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |line| {
            if !quiet {
                status.show(Some(line));
            }
        });
        let (meter, partial) = crate::chat::voice::status_feed(self.strings(), key, show);
        let session = self.live().id.to_string();
        let answered = match self.voice.kit(meter, partial, &session, true).await {
            Ok(kit) => {
                let mut host = Terminal {
                    repl: self,
                    exit: false,
                };
                pmt_voice::wake::listener::answer(&mut host, &kit).await
            }
            Err(why) => Err(why),
        };
        self.input.status_line().show(None);
        if let Err(why) = answered {
            self.notice(&why);
        }
        self.wake.resume();
        Flow::Continue
    }
}

/// The line interface as the conversation's surface.
struct Terminal<'a> {
    repl: &'a mut Repl,
    /// A typed command asked to leave.
    exit: bool,
}

impl Host for Terminal<'_> {
    async fn turn(&mut self, text: String, mouth: Option<Arc<Mouth>>, cut: CancelToken) {
        self.repl.mouth = mouth;
        self.repl.voice_cut = Some(cut);
        Box::pin(self.repl.user_turn(text)).await;
        self.repl.voice_cut = None;
        self.repl.mouth = None;
    }

    async fn interjection(&mut self) -> Interjection {
        loop {
            let prompt = self.repl.prompt();
            self.repl.input.request(&prompt);
            match self.repl.input.next().await {
                Event::Record | Event::Interrupt | Event::Eof => return Interjection::RecordKey,
                Event::Line(line) if !line.trim().is_empty() => return Interjection::Line(line),
                Event::Line(_) | Event::Compose(_) | Event::SuspendRefused => {}
            }
        }
    }

    async fn typed(&mut self, line: String) -> bool {
        if Box::pin(self.repl.idle_line(line)).await == Flow::Exit {
            self.exit = true;
            return false;
        }
        self.repl.voice.mode.on
    }

    fn heard(&mut self, transcript: &str) {
        self.repl.say(Role::Prompt, &format!("› {transcript}"));
    }
}
