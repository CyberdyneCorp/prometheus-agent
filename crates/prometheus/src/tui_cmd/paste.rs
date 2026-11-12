//! Pastes into the composer (spec: tui, Large pastes collapse into chips;
//! Clipboard image attachments): a long paste becomes a chip, `/paste` and
//! `Ctrl+V` attach the clipboard's image as a badge (its file kept under
//! `<profile>/cache/clipboard/`, stored in the profile's blob store with
//! the prompt that carries it), `Ctrl+V` pastes the clipboard's text when
//! it holds text, and `/terminal-setup` makes `Ctrl+V` reach the TUI in
//! VS Code, Cursor and Windsurf.

use std::path::PathBuf;

use pmt_i18n::t;
use pmt_rpc::contracts::PromptImage;
use pmt_tui::app::Card;
use pmt_tui::transcript::Kind;

use super::clipboard::{self, Env};
use super::screen::{Done, Tui};
use super::terminal_setup;

/// What the clipboard held.
#[derive(Debug)]
pub(super) enum Grabbed {
    /// An image (`/paste`, or `Ctrl+V` on a clipboard without text).
    Image(Vec<u8>),
    /// Text (`Ctrl+V`).
    Text(String),
    /// Nothing usable.
    Nothing,
}

impl Tui<'_> {
    /// A bracketed paste: a chip when it is long.
    pub(super) fn paste_text(&mut self, text: &str) {
        let strings = self.strings();
        self.app.composer.paste(text, self.collapse, &strings);
    }

    /// `/paste` (`text_first: false`) or `Ctrl+V` (`true`): reads the
    /// clipboard in the background.
    pub(super) fn paste_clipboard(&mut self, text_first: bool) {
        let strings = self.strings();
        let clipboard = match clipboard::detect(&Env::of(&self.open.env), &strings) {
            Ok(clipboard) => clipboard,
            Err(reason) => {
                self.notice(clipboard::unavailable(&strings, &reason));
                return;
            }
        };
        self.background(async move {
            let read = pmt_core::spawn::blocking(move || {
                if text_first && let Some(text) = clipboard.text() {
                    return Ok(Grabbed::Text(text));
                }
                let image = clipboard.image(&strings)?;
                Ok(image.map_or(Grabbed::Nothing, Grabbed::Image))
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
            Done::Clipboard(read)
        });
    }

    /// The clipboard's contents arrived.
    pub(super) fn clipboard_read(&mut self, grabbed: Result<Grabbed, String>) {
        match grabbed {
            Ok(Grabbed::Text(text)) => self.paste_text(&text),
            Ok(Grabbed::Image(bytes)) => self.attach_clipboard_image(&bytes),
            Ok(Grabbed::Nothing) => self.notice(t!(self.strings(), "tui.clip_empty")),
            Err(reason) => self.notice(clipboard::unavailable(&self.strings(), &reason)),
        }
    }

    fn attach_clipboard_image(&mut self, bytes: &[u8]) {
        let dir = self.open.scope.dir().join("cache").join("clipboard");
        let path = dir.join(format!("clipboard-{}.png", ulid::Ulid::generate()));
        let saved = pmt_core::private_fs::create_dir(&dir)
            .and_then(|()| pmt_core::private_fs::write_atomic(&path, bytes));
        if let Err(err) = saved {
            self.notice(t!(self.strings(), "tui.clip_not_kept", 0 = err));
            return;
        }
        // The same checks as `/image` (format, size).
        if let Err(err) = crate::chat::attach::load(&path) {
            let _ = std::fs::remove_file(&path);
            self.error(&err);
            return;
        }
        let strings = self.strings();
        let label = self.app.composer.attach_image(path, &strings);
        self.notice(t!(strings, "tui.image_attached", 0 = label));
    }

    /// The composer's prompt: chips expanded, badges as images.
    pub(super) fn take_prompt(&mut self) -> (String, Vec<PromptImage>) {
        let sent = self.app.composer.submit();
        let images = sent
            .images
            .into_iter()
            .map(|path: PathBuf| PromptImage::Path(path.to_string_lossy().into_owned()))
            .collect();
        (sent.text, images)
    }

    /// `/terminal-setup`: shows the change and asks.
    pub(super) fn terminal_setup(&mut self) {
        let targets = terminal_setup::targets(&self.open.env);
        if targets.is_empty() {
            self.notice(t!(self.strings(), "tui.setup_none"));
            return;
        }
        self.notice(terminal_setup::describe(&self.strings(), &targets));
        self.setup = Some(targets);
        self.show_question();
    }

    /// The card asking to apply `/terminal-setup`, when it waits.
    pub(super) fn setup_card(&self) -> Option<Card> {
        let strings = self.strings();
        self.setup.as_ref().map(|targets| Card {
            title: t!(strings, "tui.setup_question", 0 = targets.len()),
            body: String::new(),
            hint: t!(strings, "tui.yes_no"),
        })
    }

    /// The answer to `/terminal-setup`; `false` when it is no answer.
    pub(super) fn answer_setup(&mut self, line: &str) -> bool {
        let yes = match line.trim().to_lowercase().as_str() {
            "y" | "yes" => true,
            "n" | "no" => false,
            _ => return false,
        };
        let Some(targets) = self.setup.take() else {
            return false;
        };
        if yes {
            for target in &targets {
                match terminal_setup::apply(&self.strings(), target) {
                    Ok(done) => self.notice(done),
                    Err(why) => self.app.transcript.push(Kind::Error, why),
                }
            }
        } else {
            self.notice(t!(self.strings(), "tui.setup_cancelled"));
        }
        self.show_question();
        true
    }
}
