//! What takes the keys besides the composer (spec: tui, Fuzzy command
//! palette; Live session switcher; Collapsible startup banner): the
//! command palette, which only ever fills the composer; the live session
//! switcher, which shows, closes and starts sessions without restarting
//! the TUI; the startup banner's headers; and mouse clicks on them.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use pmt_rpc::contracts::{
    CommandSource, Disposition, SessionListParams, SessionListing, SessionSetup,
};
use pmt_tui::app::{Hit, Overlay};
use pmt_tui::palette::{Entry, Palette};
use pmt_tui::slash::LOCAL;
use pmt_tui::switcher::{Row, Switcher};

use super::link::{create, resume};
use super::screen::{Done, Tui};

/// What a key does inside an overlay.
enum Step {
    /// Nothing more.
    Stay,
    /// Close the overlay.
    Close,
}

impl Tui<'_> {
    /// Routes `key` to the open overlay or the focused banner; `false`
    /// when neither takes it.
    pub(super) fn overlay_key(&mut self, key: &KeyEvent) -> bool {
        let step = match self.app.overlay.take() {
            Some(Overlay::Palette(mut palette)) => {
                let step = self.palette_key(&mut palette, key);
                self.app.overlay = Some(Overlay::Palette(palette));
                step
            }
            Some(Overlay::Switcher(mut switcher)) => {
                let step = self.switcher_key(&mut switcher, key);
                self.app.overlay = Some(Overlay::Switcher(switcher));
                step
            }
            None => return self.banner_key(key),
        };
        if matches!(step, Step::Close) {
            self.app.overlay = None;
        }
        true
    }

    /// Opens the palette over the commands this surface runs, the skills
    /// and the TUI's own commands.
    pub(super) fn open_palette(&mut self) {
        let daemon = self
            .link
            .commands
            .iter()
            .filter(|c| c.disposition == Disposition::Exec)
            .map(|c| Entry {
                name: c.name.clone(),
                description: c.description.clone(),
                disposition: if c.source == CommandSource::Skill {
                    "skill"
                } else {
                    "exec"
                }
                .to_owned(),
            });
        let local = LOCAL.iter().map(|c| Entry {
            name: c.name.to_owned(),
            description: c.description.to_owned(),
            disposition: "client".to_owned(),
        });
        self.app.overlay = Some(Overlay::Palette(Palette::new(
            local.chain(daemon).collect(),
        )));
    }

    fn palette_key(&mut self, palette: &mut Palette, key: &KeyEvent) -> Step {
        match key.code {
            KeyCode::Esc => return Step::Close,
            KeyCode::Enter => {
                if let Some(chosen) = palette.chosen() {
                    // Inserted, never run: the person sends it.
                    self.app.composer.set(&chosen);
                }
                return Step::Close;
            }
            KeyCode::Up => palette.move_selection(-1),
            KeyCode::Down => palette.move_selection(1),
            KeyCode::Backspace => palette.pop(),
            KeyCode::Char(c) if typed(key) => palette.push(c),
            _ => {}
        }
        Step::Stay
    }

    /// Opens the switcher over the live sessions and asks for their
    /// titles.
    pub(super) fn open_switcher(&mut self) {
        self.app.overlay = Some(Overlay::Switcher(Switcher::new(self.switcher_rows())));
        self.refresh_titles();
    }

    fn switcher_rows(&self) -> Vec<Row> {
        self.link
            .live
            .iter()
            .map(|id| Row {
                id: id.clone(),
                title: self.titles.get(id).cloned(),
                busy: self.pending.get(id).is_some_and(|t| !t.is_empty()),
                shown: *id == self.link.session,
            })
            .collect()
    }

    /// Asks the daemon for the sessions' titles.
    fn refresh_titles(&self) {
        let client = self.link.client.clone();
        self.background(async move {
            let params = SessionListParams::default();
            let listed = client.call_typed::<SessionListing>(&params).await;
            let titles: HashMap<String, String> = listed
                .map(|list| {
                    list.sessions
                        .into_iter()
                        .filter_map(|s| s.title.map(|t| (s.session_id, t)))
                        .collect()
                })
                .unwrap_or_default();
            Done::Titles(titles)
        });
    }

    /// Titles arrived: the open switcher shows them.
    pub(super) fn titles_listed(&mut self, titles: HashMap<String, String>) {
        self.titles = titles;
        let rows = self.switcher_rows();
        if let Some(Overlay::Switcher(switcher)) = &mut self.app.overlay {
            switcher.rows = rows;
            switcher.selected = switcher.selected.min(switcher.rows.len());
        }
    }

    fn switcher_key(&mut self, switcher: &mut Switcher, key: &KeyEvent) -> Step {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return Step::Close,
            KeyCode::Up => switcher.move_selection(-1),
            KeyCode::Down => switcher.move_selection(1),
            KeyCode::Tab => switcher.next_model(&self.models),
            KeyCode::Enter => return self.switcher_enter(switcher),
            KeyCode::Char('n') if ctrl => {
                self.start_session(None, None);
                return Step::Close;
            }
            KeyCode::Char('r') if ctrl => self.refresh_titles(),
            KeyCode::Char('d') if ctrl => self.close_selected(switcher),
            KeyCode::Backspace => {
                if let Some(draft) = &mut switcher.draft {
                    draft.prompt.pop();
                }
            }
            KeyCode::Char(c) if typed(key) => switcher.type_char(c),
            _ => {}
        }
        Step::Stay
    }

    fn switcher_enter(&mut self, switcher: &Switcher) -> Step {
        if switcher.on_new() {
            let draft = switcher.draft.clone().unwrap_or_default();
            let prompt = Some(draft.prompt).filter(|p| !p.trim().is_empty());
            self.start_session(draft.model, prompt);
            return Step::Close;
        }
        if let Some(row) = switcher.current()
            && !row.shown
        {
            let id = row.id.clone();
            self.show_session(id);
        }
        Step::Close
    }

    /// `Ctrl+D`: detaches from the highlighted session and drops it from
    /// the list (never the last one).
    fn close_selected(&mut self, switcher: &mut Switcher) {
        let Some(row) = switcher.current().cloned() else {
            return;
        };
        if self.link.live.len() < 2 {
            self.notice("the only live session stays; /quit leaves");
            return;
        }
        switcher.rows.retain(|r| r.id != row.id);
        switcher.selected = switcher.selected.min(switcher.rows.len().saturating_sub(1));
        let _ = self.done.send(Done::Close(row.id));
    }

    /// Shows the live session `id` without restarting anything.
    fn show_session(&self, id: String) {
        let _ = self.done.send(Done::Show(id));
    }

    /// `Done::Show`: the screen follows the live session `id`.
    pub(super) async fn shown(&mut self, id: &str) {
        self.link.show(id).await;
        self.shown_changed();
        self.reload().await;
        self.notice(format!("session {id}"));
    }

    /// `Done::Close`: detaches from `id`; another live session is shown
    /// when it was the shown one.
    pub(super) async fn closed(&mut self, id: &str) {
        let was_shown = self.link.session == id;
        let summary = crate::daemon_client::memory_summary(&self.link.client, id).await;
        self.link.close(id).await;
        self.pending.remove(id);
        self.shown_changed();
        if was_shown {
            self.reload().await;
        }
        self.notice(format!("closed session {id}"));
        if let Some(line) = summary {
            self.notice(line);
        }
    }

    /// Starts a new session (with `model` for it alone) and shows it,
    /// submitting `prompt` there; the sessions shown before keep running.
    pub(super) fn start_session(&self, model: Option<String>, prompt: Option<String>) {
        let client = self.link.client.clone();
        let mut launch = self.open.launch.clone();
        if model.is_some() {
            launch.setup = SessionSetup {
                model,
                provider: None,
                ..launch.setup
            };
        }
        self.background(async move {
            let opened = match create(&client, &launch).await {
                Ok(id) => resume(&client, &id, None).await.map(|a| (id, a)),
                Err(err) => Err(err),
            };
            Done::Switched(opened, prompt)
        });
    }

    /// `Ctrl+B`, then `Up`/`Down` and `Enter` on the banner's headers;
    /// `Esc` or any other key leaves them.
    fn banner_key(&mut self, key: &KeyEvent) -> bool {
        let Some(banner) = &mut self.app.banner else {
            return false;
        };
        let Some(focus) = banner.focus else {
            return false;
        };
        match key.code {
            KeyCode::Up => banner.move_focus(-1),
            KeyCode::Down => banner.move_focus(1),
            KeyCode::Enter => banner.toggle(focus),
            _ => {
                banner.focus = None;
                return key.code == KeyCode::Esc;
            }
        }
        true
    }

    /// The banner key: moves to the banner's first header (and away).
    pub(super) fn focus_banner(&mut self) {
        if let Some(banner) = &mut self.app.banner {
            banner.focus = match banner.focus {
                Some(_) => None,
                None => Some(0),
            };
            self.app.scroll = usize::MAX;
        }
    }

    /// A click: a banner header toggles, `<n> live` opens the switcher.
    pub(super) fn on_click(&mut self, mouse: MouseEvent) {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }
        match self.app.hit(mouse.column, mouse.row) {
            Some(Hit::BannerSection(index)) => {
                if let Some(banner) = &mut self.app.banner {
                    banner.toggle(index);
                }
            }
            Some(Hit::Live) => self.open_switcher(),
            None => {}
        }
    }
}

/// A plain or shifted character.
fn typed(key: &KeyEvent) -> bool {
    key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
}
