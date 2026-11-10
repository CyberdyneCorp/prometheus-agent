//! Keys, pastes and mouse wheel; what a submitted line does (spec: tui,
//! Slash command flow; Composer and path completion; Configurable key
//! bindings): an answer to the card, a client-local command run here
//! without a round-trip, a command for `command.dispatch`, or a prompt.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use pmt_rpc::contracts::{
    CommandDispatch, CompleteParams, CompletePath, CompleteSlash, DispatchParams, ResolveKind,
    SessionInterrupt, SessionRef,
};
use pmt_tui::keys::Action;
use pmt_tui::slash::{Input, LOCAL, Local, classify};

use super::Ending;
use super::screen::{Done, Tui};

/// Lines a page scroll moves.
const PAGE: usize = 10;

impl Tui<'_> {
    /// One terminal event.
    pub(super) fn on_terminal(&mut self, event: Event) {
        match event {
            Event::Key(key) if self.masked_front() => self.masked_key(&key),
            Event::Key(key) => self.on_key(&key),
            Event::Paste(text) if self.masked_front() => self.masked_paste(&text),
            Event::Paste(text) => self.paste_text(&text),
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => self.app.scroll_up(3),
                MouseEventKind::ScrollDown => self.app.scroll_down(3),
                _ => self.on_click(mouse),
            },
            _ => {}
        }
    }

    fn on_key(&mut self, key: &KeyEvent) {
        if self.overlay_key(key) {
            return;
        }
        // Cmd+V, on terminals that report the Super modifier.
        if key.code == KeyCode::Char('v') && key.modifiers.contains(KeyModifiers::SUPER) {
            self.paste_clipboard(true);
            return;
        }
        // In voice mode the record key talks, whatever else it is bound to.
        if self.voice.on && self.is_record_key(key) {
            self.record_key();
            return;
        }
        if let Some(action) = self.open.bindings.action(key) {
            self.on_action(action);
            return;
        }
        self.app.completions.clear();
        let typed = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
        let composer = &mut self.app.composer;
        match key.code {
            KeyCode::Char(c) if typed => {
                // An approval is answered by its letter on an empty composer.
                if composer.is_empty() && self.answer_key(c) {
                    return;
                }
                self.app.composer.insert(c);
            }
            KeyCode::Backspace => composer.backspace(),
            KeyCode::Delete => composer.delete(),
            KeyCode::Left => composer.left(),
            KeyCode::Right => composer.right(),
            KeyCode::Home => composer.line_start(),
            KeyCode::End => composer.line_end(),
            KeyCode::Esc => self.app.completions.clear(),
            _ => {}
        }
    }

    /// `o`/`s`/`a`/`d`/`e` on an empty composer answers an approval card,
    /// `y`/`n` the `/terminal-setup` card.
    fn answer_key(&mut self, c: char) -> bool {
        let approval = self
            .asking
            .front()
            .is_some_and(|asked| asked.method() == "approval.request");
        if self.asking.is_empty() && self.setup.is_some() && "yn".contains(c) {
            return self.answer_setup(&c.to_string());
        }
        approval && "osade".contains(c) && self.answer(&c.to_string())
    }

    pub(super) fn on_action(&mut self, action: Action) {
        if action != Action::Complete {
            self.app.completions.clear();
        }
        let composer = &mut self.app.composer;
        match action {
            Action::Submit => self.submit_line(),
            Action::Newline => composer.insert('\n'),
            Action::Interrupt => self.interrupt(),
            Action::Quit if composer.is_empty() => self.ending = Some(Ending::Quit),
            Action::Quit => composer.delete(),
            Action::ScrollUp => self.app.scroll_up(1),
            Action::ScrollDown => self.app.scroll_down(1),
            Action::PageUp => self.app.scroll_up(PAGE),
            Action::PageDown => self.app.scroll_down(PAGE),
            Action::HistoryPrev => {
                let current = composer.text().to_owned();
                if let Some(entry) = self.history.previous(&current) {
                    self.app.composer.set(entry);
                }
            }
            Action::HistoryNext => {
                if let Some(entry) = self.history.following() {
                    self.app.composer.set(entry);
                }
            }
            Action::Complete => self.complete(),
            Action::LineStart => composer.line_start(),
            Action::LineEnd => composer.line_end(),
            Action::ClearInput => {
                composer.take();
            }
            Action::Palette => self.open_palette(),
            Action::Sessions => self.open_switcher(),
            Action::PasteImage => self.paste_clipboard(true),
            Action::ExpandPaste => {
                composer.expand();
            }
            Action::Banner => self.focus_banner(),
        }
    }

    /// Ctrl+C: stops the running turn, else clears the composer.
    fn interrupt(&mut self) {
        if self.app.status.busy {
            let params = SessionRef {
                session_id: self.link.session.clone(),
            };
            let client = self.link.client.clone();
            tokio::spawn(async move {
                let _ = client.call_typed::<SessionInterrupt>(&params).await;
            });
            self.notice("interrupting the running turn");
        } else if self.app.composer.is_empty() {
            self.notice("Ctrl+D or /quit leaves");
        } else {
            self.app.composer.take();
        }
    }

    fn submit_line(&mut self) {
        let line = self.app.composer.text().to_owned();
        let answered = if self.asking.is_empty() {
            (self.setup.is_some() && self.answer_setup(&line))
                || (self.prune && self.answer_prune(&line))
        } else {
            self.answer(line.trim())
        };
        if answered {
            self.app.composer.take();
            return;
        }
        let (line, images) = self.take_prompt();
        if line.trim().is_empty() && images.is_empty() {
            return;
        }
        self.app.scroll = 0;
        if let Err(err) = self.history.push(&line) {
            tracing::warn!("cannot write the TUI history: {err}");
        }
        if !images.is_empty() {
            // Images go with a prompt, never with a command.
            self.submit(line, images);
            return;
        }
        // A voice conversation listens: it hears a stop phrase, and hands
        // anything else back to run as typed.
        if !self.voice_line(&line) {
            self.run_line(&line);
        }
    }

    /// A typed line: a client-local command, a command for the daemon, or
    /// a prompt.
    pub(super) fn run_line(&mut self, line: &str) {
        match classify(line.trim_end(), |name| self.link.registered(name)) {
            Input::Local(command, args) => self.local(command, &args),
            Input::Dispatch { name, args } => self.dispatch(name, args),
            Input::Prompt(text) => self.submit(text, Vec::new()),
        }
    }

    /// Whether `key` is `voice.record_key`.
    fn is_record_key(&self, key: &KeyEvent) -> bool {
        let configured = &self.open.scope.config().voice.record_key.0;
        pmt_tui::keys::chord_of(key).is_some_and(|chord| {
            pmt_config::keys::Chord::parse(configured).is_ok_and(|record| record == chord)
        }) && key.kind != crossterm::event::KeyEventKind::Release
    }

    pub(super) fn dispatch(&self, name: String, args: String) {
        let params = DispatchParams {
            session_id: self.link.session.clone(),
            name,
            args,
        };
        let client = self.link.client.clone();
        self.background(async move {
            Done::Dispatched(
                client
                    .call_typed::<CommandDispatch>(&params)
                    .await
                    .map_err(|e| pmt_rpc::client::error_of(&e)),
            )
        });
    }

    /// A client-local command: no round-trip for the screen's own ones.
    fn local(&mut self, command: Local, args: &str) {
        match command {
            Local::Quit => self.ending = Some(Ending::Quit),
            Local::Clear => {
                self.app.transcript.clear();
                self.app.scroll = 0;
            }
            Local::Copy => self.copy(),
            Local::Theme => self.theme(args),
            Local::Redraw => self.redraw(),
            Local::StatusBar => self.statusbar(args),
            Local::Battery => self.battery(args),
            Local::Indicator => self.indicator(args),
            Local::Palette => self.open_palette(),
            Local::Sessions if args.trim() == "new" => self.start_session(None, None),
            Local::Sessions => self.open_switcher(),
            Local::Paste => self.paste_clipboard(false),
            Local::TerminalSetup => self.terminal_setup(),
            Local::Keys => {
                let keys = self.open.bindings.describe();
                self.notice(keys);
            }
            Local::Update => {
                let words = args.split_whitespace().map(str::to_owned).collect();
                self.ending = Some(Ending::Update(words));
            }
            Local::New | Local::Resume | Local::Fork => self.switch(command, args),
            Local::Handoff => self.handoff(args),
            Local::Voice => self.voice_command(args),
        }
    }

    /// `/handoff <destination>`: refused while a turn runs; once the
    /// daemon moved the session, the TUI leaves.
    fn handoff(&mut self, args: &str) {
        if self.app.status.busy {
            self.notice("/handoff is unavailable while the agent is working; stop it first");
            return;
        }
        let home = self.open.home.clone();
        let profile = self.open.scope.name().to_string();
        let session = self.link.session.clone();
        let target = args.to_owned();
        self.background(async move {
            Done::HandedOff(crate::chat::handoff::handoff(&home, &profile, &session, &target).await)
        });
    }

    /// `/copy`: the last answer to the system clipboard with OSC 52, which
    /// terminals (also over SSH) pass to their own clipboard.
    fn copy(&mut self) {
        use base64::Engine as _;
        use std::io::Write as _;
        let Some(answer) = self.app.transcript.last_answer().map(str::to_owned) else {
            self.notice("nothing to copy yet");
            return;
        };
        let encoded = base64::engine::general_purpose::STANDARD.encode(answer.as_bytes());
        let mut out = std::io::stdout();
        let _ = write!(out, "\u{1b}]52;c;{encoded}\u{7}");
        let _ = out.flush();
        self.notice("copied the last answer");
    }

    /// `/new`, `/resume <ref>`, `/fork`: opens another session and shows
    /// it; the one shown before stays live (the switcher lists both).
    fn switch(&mut self, command: Local, args: &str) {
        use super::link::{create, fork, resolve, resume};
        let client = self.link.client.clone();
        let session = self.link.session.clone();
        let launch = self.open.launch.clone();
        let reference = args.trim().to_owned();
        self.background(async move {
            let opened = match command {
                Local::New => create(&client, &launch).await,
                Local::Fork => fork(&client, &session).await,
                _ => resolve(&client, ResolveKind::Resume, reference, &launch).await,
            };
            let switched = match opened {
                Ok(id) => resume(&client, &id, None)
                    .await
                    .map(|attached| (id, attached)),
                Err(err) => Err(err),
            };
            Done::Switched(switched, None)
        });
    }

    /// Tab: completes the `/command` or `@path` at the cursor.
    fn complete(&mut self) {
        let (start, word) = self.app.composer.word_at_cursor();
        let first_word = start == 0;
        let (prefix, path) = match word {
            w if first_word && w.starts_with('/') => (w.to_owned(), false),
            w if w.starts_with('@') => (w[1..].to_owned(), true),
            _ => return,
        };
        let params = CompleteParams {
            session_id: self.link.session.clone(),
            prefix,
        };
        let client = self.link.client.clone();
        let offset = if path { start + 1 } else { start };
        let local = if path {
            Vec::new()
        } else {
            local_matches(word)
        };
        self.background(async move {
            let mut found = if path {
                client.call_typed::<CompletePath>(&params).await
            } else {
                client.call_typed::<CompleteSlash>(&params).await
            }
            .unwrap_or_default();
            found.items.splice(0..0, local);
            Done::Completed(offset, found)
        });
    }

    /// Applies completions for the word at `start`: one is inserted, several
    /// are listed and their common prefix inserted.
    pub(super) fn completed(&mut self, start: usize, found: &pmt_rpc::contracts::Completions) {
        let (word_start, word) = self.app.composer.word_at_cursor();
        let typed = &word[start.saturating_sub(word_start).min(word.len())..];
        let texts: Vec<&str> = found.items.iter().map(|c| c.text.as_str()).collect();
        let Some(common) = common_prefix(&texts) else {
            return;
        };
        if common.len() > typed.len() {
            let mut with = common.to_owned();
            if texts.len() == 1 && !found.items[0].dir {
                with.push(' ');
            }
            self.app.composer.replace_word(start, &with);
        }
        if texts.len() > 1 {
            self.app.completions = found
                .items
                .iter()
                .map(|c| match &c.description {
                    Some(d) => format!("{}  {d}", c.text),
                    None => c.text.clone(),
                })
                .collect();
        }
    }
}

/// The TUI's own commands whose name starts with `typed` (`/` included).
fn local_matches(typed: &str) -> Vec<pmt_rpc::contracts::Completion> {
    let name = typed.trim_start_matches('/');
    LOCAL
        .iter()
        .filter(|c| c.name.starts_with(name))
        .map(|c| pmt_rpc::contracts::Completion {
            text: format!("/{}", c.name),
            description: Some(c.description.to_owned()),
            dir: false,
        })
        .collect()
}

/// The longest prefix every text shares; `None` for no text.
fn common_prefix<'a>(texts: &[&'a str]) -> Option<&'a str> {
    let first = *texts.first()?;
    let mut end = first.len();
    for text in &texts[1..] {
        end = first
            .char_indices()
            .zip(text.chars())
            .take_while(|((_, a), b)| a == b)
            .last()
            .map_or(0, |((at, c), _)| at + c.len_utf8())
            .min(end);
    }
    Some(&first[..end])
}

#[cfg(test)]
mod tests {
    use super::common_prefix;

    #[test]
    fn completions_share_their_common_prefix() {
        assert_eq!(common_prefix(&["/compact", "/compress"]), Some("/comp"));
        assert_eq!(common_prefix(&["./src/main.rs"]), Some("./src/main.rs"));
        assert_eq!(common_prefix(&["a", "b"]), Some(""));
        assert_eq!(common_prefix(&[]), None);
    }
}
