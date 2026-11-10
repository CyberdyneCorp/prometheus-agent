//! Voice mode in the line interface (spec: voice-mode, Voice mode toggle
//! and commands; Push-to-talk recording and continuous loop; Spoken and
//! typed stop phrases). `/voice` arms the record key; pressing it at the
//! idle prompt starts the continuous conversation of [`pmt_voice`], whose
//! transcripts run as ordinary user turns and whose replies are spoken
//! sentence by sentence when spoken replies are on. The level meter is
//! the bottom status line while a recording runs.

use std::sync::Arc;

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
        let meter: pmt_voice::record::Meter = Arc::new(move |level| {
            if !quiet {
                status.show(Some(crate::chat::voice::meter_line(level, &key)));
            }
        });
        let kit = match self.voice.kit(meter).await {
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

/// The line interface as the conversation's surface.
struct Terminal<'a> {
    repl: &'a mut Repl,
    /// A typed command asked to leave.
    exit: bool,
}

impl Host for Terminal<'_> {
    async fn turn(&mut self, text: String, mouth: Option<Arc<Mouth>>) {
        self.repl.mouth = mouth;
        Box::pin(self.repl.user_turn(text)).await;
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
