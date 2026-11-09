//! Voice mode in the attached line interface (spec: voice-mode, Voice
//! mode toggle and commands; Push-to-talk recording and continuous loop):
//! the same conversation as the in-process line interface, recording,
//! transcribing and speaking on this machine while the daemon runs the
//! turns. The daemon sends whole messages, so a reply is spoken once it
//! is complete.

use std::sync::Arc;

use pmt_core::error::Error;
use pmt_voice::converse::{self, Host, Interjection};
use pmt_voice::speaker::Mouth;

use super::line::{Chat, Flow, Typed};
use super::render::Out as _;
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
        let meter: pmt_voice::record::Meter = Arc::new(move |level| {
            if !quiet {
                status.show(Some(crate::chat::voice::meter_line(level, &key)));
            }
        });
        let kit = match self.voice.kit(meter).await {
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

/// The attached line interface as the conversation's surface.
struct Attached<'c, 'a, 'b> {
    chat: &'c mut Chat<'a, 'b>,
    exit: bool,
    /// The connection ended: the conversation stops and the error ends
    /// the interface.
    lost: Option<Error>,
}

impl Host for Attached<'_, '_, '_> {
    async fn turn(&mut self, text: String, mouth: Option<Arc<Mouth>>) {
        self.chat.mouth = mouth;
        let sent = Box::pin(self.chat.submit(text)).await;
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
            Ok(Flow::Exit) => {
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
