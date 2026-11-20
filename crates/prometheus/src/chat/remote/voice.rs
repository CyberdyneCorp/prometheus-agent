//! Voice mode in the attached line interface (spec: voice-mode, Voice
//! mode toggle and commands; Push-to-talk recording and continuous loop):
//! the same conversation as the in-process line interface, recording,
//! transcribing and speaking on this machine while the daemon runs the
//! turns. The reply is spoken while it generates, from the turn's
//! `assistant.delta` events.

use std::sync::Arc;

use pmt_core::cancel::CancelToken;
use pmt_core::error::Error;
use pmt_voice::converse::{self, Host, Interjection};
use pmt_voice::speaker::Mouth;

use super::line::{Chat, Flow, Screen, Typed};
use super::render::Out;
use crate::chat::theme::Role;

impl Chat<'_, '_> {
    /// `/voice [on|off|tts|status]`.
    pub(super) async fn voice_command(&mut self, args: &str) {
        let said = self.voice.command(args).await;
        self.lines.set_voice(self.voice.mode.on);
        self.screen.notice(&said);
    }

    /// The record key at the idle prompt: a voice conversation until it
    /// ends.
    ///
    /// # Errors
    ///
    /// The connection to the daemon was lost.
    pub(super) async fn talk(&mut self) -> Result<Flow, Error> {
        let status = self.lines.status_line();
        let quiet = self.start.ui.quiet;
        let key = pmt_voice::mode::record_key(&self.start.scope.config().voice);
        let show: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |line| {
            if !quiet {
                status.show(Some(line));
            }
        });
        let (meter, partial) = crate::chat::voice::status_feed(self.strings(), key, show);
        let session = self.attached.session.clone();
        let spoken = self.voice.mode.spoken;
        let kit = match self.voice.kit(meter, partial, &session, spoken).await {
            Ok(kit) => kit,
            Err(why) => {
                self.screen.notice(&why);
                return Ok(Flow::Continue);
            }
        };
        let mut host = Attached {
            chat: self,
            exit: false,
            lost: None,
        };
        let ended = converse::converse(&mut host, &kit).await;
        let (exit, lost) = (host.exit, host.lost);
        self.lines.status_line().show(None);
        if let Some(err) = lost {
            return Err(err);
        }
        self.screen.notice(&ended.message());
        Ok(if exit { Flow::Exit } else { Flow::Continue })
    }
}

impl Chat<'_, '_> {
    /// `/wake [on|off|status]`.
    pub(super) async fn wake_command(&mut self, args: &str) {
        let woken = Arc::clone(&self.woken.0);
        let said = self.wake.command(args, &mut self.voice, &woken).await;
        self.screen.notice(&said);
    }

    /// Arms the wake listener at startup when `wake.enabled` is set.
    pub(super) async fn arm_wake(&mut self) {
        let woken = Arc::clone(&self.woken.0);
        if let Some(said) = self.wake.start(&mut self.voice, &woken).await {
            self.screen.notice(&said);
        }
    }

    /// The wake phrase was heard at the idle prompt: a new session (unless
    /// `wake.start_new_session = false`), one spoken request answered
    /// aloud, then listening resumes.
    ///
    /// # Errors
    ///
    /// The connection to the daemon was lost.
    pub(super) async fn woken(
        &mut self,
        detection: pmt_voice::wake::engine::Detection,
    ) -> Result<Flow, Error> {
        if let Some(elsewhere) = self.wake.elsewhere(&detection) {
            self.screen.notice(&elsewhere);
            self.wake.resume();
            return Ok(Flow::Continue);
        }
        self.screen
            .notice(&format!("Heard \"{}\".", detection.phrase));
        if self.start.scope.config().wake.start_new_session {
            let created = super::open::claim(&self.attached.client, None, self.start).await;
            self.switch(created).await;
        }
        let status = self.lines.status_line();
        let quiet = self.start.ui.quiet;
        let key = pmt_voice::mode::record_key(&self.start.scope.config().voice);
        let show: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |line| {
            if !quiet {
                status.show(Some(line));
            }
        });
        let (meter, partial) = crate::chat::voice::status_feed(self.strings(), key, show);
        let session = self.attached.session.clone();
        let kit = self.voice.kit(meter, partial, &session, true).await;
        let (answered, lost) = match kit {
            Ok(kit) => {
                let mut host = Attached {
                    chat: self,
                    exit: false,
                    lost: None,
                };
                let answered = pmt_voice::wake::listener::answer(&mut host, &kit).await;
                (answered, host.lost)
            }
            Err(why) => (Err(why), None),
        };
        self.lines.status_line().show(None);
        if let Some(err) = lost {
            return Err(err);
        }
        if let Err(why) = answered {
            self.screen.notice(&why);
        }
        self.wake.resume();
        Ok(Flow::Continue)
    }
}

/// The attached line interface as the conversation's surface.
struct Attached<'c, 'a, 'b> {
    chat: &'c mut Chat<'a, 'b>,
    exit: bool,
    /// The connection ended: the conversation stops and the error ends
    /// the interface.
    lost: Option<Error>,
}

impl Host for Attached<'_, '_, '_> {
    async fn turn(&mut self, text: String, mouth: Option<Arc<Mouth>>, cut: CancelToken) {
        self.chat.mouth = mouth;
        self.chat.voice_cut = Some(cut);
        let sent = Box::pin(self.chat.submit(text)).await;
        self.chat.voice_cut = None;
        self.chat.mouth = None;
        if let Err(err) = sent {
            self.lost = Some(err);
        }
    }

    async fn interjection(&mut self) -> Interjection {
        if self.lost.is_some() {
            return Interjection::RecordKey;
        }
        loop {
            match self.chat.lines.next().await {
                Typed::Record | Typed::Interrupt | Typed::Eof => return Interjection::RecordKey,
                Typed::Line(line) if !line.trim().is_empty() => return Interjection::Line(line),
                Typed::Line(_) | Typed::Compose(_) => {}
            }
        }
    }

    async fn typed(&mut self, line: String) -> bool {
        match Box::pin(self.chat.line(line)).await {
            // A spoken session leaves without deleting (`/exit --delete`
            // is typed at the prompt).
            Ok(Flow::Exit | Flow::ExitDelete) => {
                self.exit = true;
                false
            }
            Ok(Flow::Continue) => self.chat.voice.mode.on,
            Err(err) => {
                self.lost = Some(err);
                false
            }
        }
    }

    fn heard(&mut self, transcript: &str) {
        self.chat
            .screen
            .say(Role::Prompt, &format!("› {transcript}"));
    }
}

/// The screen of a voice turn whose reply is spoken: the answer's pieces
/// go to the mouth as they arrive, everything else to the screen.
pub(super) struct Speaking<'s> {
    pub(super) screen: &'s Screen,
    pub(super) mouth: &'s Mouth,
}

impl Out for Speaking<'_> {
    fn tool(&self, text: &str) {
        self.screen.tool(text);
    }

    fn notice(&self, text: &str) {
        self.screen.notice(text);
    }

    fn ask(&self, text: &str) {
        self.screen.ask(text);
    }

    fn answer(&self, text: &str) {
        self.screen.answer(text);
    }

    fn delta(&self, text: &str) {
        self.mouth.text(text);
    }

    fn error(&self, err: &Error) {
        self.screen.error(err);
    }
}
