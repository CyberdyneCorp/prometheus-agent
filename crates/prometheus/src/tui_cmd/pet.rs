//! The TUI's pet, `/pet`, `/hatch` and achievement toasts (spec: pets,
//! Terminal pet rendering; Pet slash command; Generating a pet;
//! Affection reactions; Achievement unlocks celebrate). Everything here is
//! client-side: config patches, the gallery and the image backend are
//! reached from this process, and no turn starts.

use std::io::Write as _;
use std::time::Instant;

use pmt_core::error::Error;
use pmt_pets::hatch::{Hatch, Step};
use pmt_tui::app::Overlay;
use pmt_tui::pet::{PetView, Toast};
use pmt_tui::pet_picker::{PetChoice, PetPicker};
use serde_json::Value;

use super::overlays::Step as Key;
use super::screen::{Done, Tui};
use crate::pets_host::slash::{self, Pet};

impl Tui<'_> {
    /// A frame passed: the pet moves on (and follows config changes).
    pub(super) fn pet_tick(&mut self) {
        let now = Instant::now();
        self.pet.tick(now);
        self.app.pet = self.pet_view(now);
    }

    fn pet_view(&self, now: Instant) -> Option<PetView> {
        let renderer = self.pet.renderer()?;
        let heart = self.pet.heart(now);
        if renderer.mode().graphics() {
            let sprite = self.pet.sprite(now)?;
            return Some(PetView {
                cells: Vec::new(),
                graphics: Some((sprite.cols, sprite.rows)),
                heart,
            });
        }
        Some(PetView {
            cells: self.pet.cells(now),
            graphics: None,
            heart,
        })
    }

    /// After a frame: a graphics pet is drawn into the box the screen
    /// left for it, when its frame or place changed (an iTerm2 or sixel
    /// image is blanked first: its transparent pixels would show the last
    /// frame).
    pub(super) fn draw_pet_graphics(&mut self) {
        let (Some(area), Some(renderer)) = (self.app.pet_area(), self.pet.renderer()) else {
            self.pet_drawn = None;
            return;
        };
        let mode = renderer.mode();
        if !mode.graphics() {
            return;
        }
        let Some(sprite) = self.pet.sprite(Instant::now()) else {
            return;
        };
        let (row, col) = (area.y + 1, area.x + 1);
        let mut text = String::new();
        if mode != pmt_pets::render::Mode::Kitty {
            text.push_str(&pmt_pets::render::clear_at(
                row,
                col,
                area.width,
                area.height,
            ));
        }
        text.push_str(&pmt_pets::render::draw_at(&sprite, row, col));
        if self.pet_drawn.as_ref() == Some(&text) {
            return;
        }
        let mut out = std::io::stdout();
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
        self.pet_drawn = Some(text);
    }

    /// The TUI ends: a graphics pet leaves the terminal with it.
    pub(super) fn remove_pet(&self) {
        let delete = self.pet.delete();
        if !delete.is_empty() {
            let mut out = std::io::stdout();
            let _ = out.write_all(delete.as_bytes());
            let _ = out.flush();
        }
    }

    /// A trajectory event of the shown session.
    pub(super) fn pet_event(&mut self, data: &Value) {
        if let Some(kind) = data["type"].as_str() {
            self.pet.event(kind, &data["payload"]);
        }
    }

    /// `achievement.unlocked`: a toast, and the pet jumps (spec: pets,
    /// Achievement unlocks celebrate). Nothing reaches the session.
    pub(super) fn on_achievement(&mut self, data: &Value) {
        let id = data["id"].as_str().unwrap_or_default();
        let tier = data["tier"].as_str().unwrap_or_default();
        let name = pmt_pets::achievements::catalog::find(id).map_or(id, |a| a.name);
        self.app.toast = Some(Toast {
            title: self.strings().get("tui.achievement_unlocked").to_owned(),
            body: format!("{name} · {tier}"),
        });
        self.pet.tracker.celebrate(Instant::now());
    }

    /// `/pet [list|off|scale <factor>|<slug>]`.
    pub(super) fn pet_command(&mut self, args: &str) {
        let pet = Pet::parse(args);
        if pet == Pet::List {
            return self.open_pet_picker();
        }
        match slash::run(self.open.scope, &pet) {
            Ok(line) => self.notice(line),
            Err(err) => self.error(&err),
        }
        self.pet_changed();
    }

    fn pet_changed(&mut self) {
        let delete = self.pet.delete();
        let graphics = self.pet.renderer().is_some_and(|r| r.mode().graphics());
        self.pet.refresh(true, Instant::now());
        if self.pet.renderer().is_none() && graphics {
            let mut out = std::io::stdout();
            let _ = out.write_all(delete.as_bytes());
            let _ = out.flush();
            // An image in the cells stays until they are written again.
            self.clear_next = true;
        }
        self.pet_drawn = None;
        self.app.pet = self.pet_view(Instant::now());
    }

    fn open_pet_picker(&mut self) {
        let scope = self.open.scope;
        let config = crate::pets_host::current(scope);
        let store = crate::pets_host::pets(scope);
        let current = config
            .enabled
            .then(|| store.active(&config.slug).map(|p| p.meta.slug))
            .flatten();
        let installed = store
            .installed()
            .into_iter()
            .map(|p| PetChoice {
                slug: p.meta.slug,
                name: p.meta.name,
                installed: true,
            })
            .collect();
        let loading = self.strings().get("tui.pet_gallery_loading").to_owned();
        self.app.overlay = Some(Overlay::Pets(PetPicker::new(installed, current, &loading)));
        let url = config.gallery_url;
        let client = self.http();
        self.background(async move {
            let listed = match client {
                Ok(client) => pmt_pets::gallery::fetch(&client, &url).await,
                Err(err) => Err(err),
            };
            Done::PetGallery(listed.map(|entries| {
                entries
                    .into_iter()
                    .map(|e| PetChoice {
                        slug: e.slug,
                        name: e.name,
                        installed: false,
                    })
                    .collect()
            }))
        });
    }

    fn http(&self) -> Result<reqwest::Client, Error> {
        let network = &self.open.scope.config().network;
        pmt_net::client(&pmt_net::OutboundPolicy {
            force_ipv4: network.force_ipv4,
            extra_ca_file: network.extra_ca_file.as_ref().map(Into::into),
            proxy: network.proxy.clone(),
        })
    }

    /// `Done::PetGallery`: the gallery joins the open picker.
    pub(super) fn pet_gallery(&mut self, listed: Result<Vec<PetChoice>, Error>) {
        if let Some(Overlay::Pets(picker)) = &mut self.app.overlay {
            match listed {
                Ok(gallery) => picker.add_gallery(gallery),
                Err(err) => picker.gallery = Some(err.message().to_owned()),
            }
        }
    }

    /// A key in the pet picker.
    pub(super) fn pet_key(
        &mut self,
        picker: &mut PetPicker,
        key: &crossterm::event::KeyEvent,
    ) -> Key {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Esc => return Key::Close,
            KeyCode::Up => picker.move_selection(-1),
            KeyCode::Down => picker.move_selection(1),
            KeyCode::Enter => {
                if let Some(chosen) = picker.chosen().cloned() {
                    self.pick_pet(chosen);
                }
                return Key::Close;
            }
            _ => {}
        }
        Key::Stay
    }

    fn pick_pet(&mut self, chosen: PetChoice) {
        if chosen.installed {
            return self.pet_command(&chosen.slug);
        }
        let scope = self.open.scope.clone();
        let client = self.http();
        self.background(async move {
            let installed = async {
                let client = client?;
                let url = crate::pets_host::current(&scope).gallery_url;
                let entries = pmt_pets::gallery::fetch(&client, &url).await?;
                let entry = entries
                    .into_iter()
                    .find(|e| e.slug == chosen.slug)
                    .ok_or_else(|| pmt_pets::store::not_found(&chosen.slug))?;
                let bytes = pmt_pets::gallery::download(&client, &entry).await?;
                let meta = pmt_pets::PetMeta {
                    slug: entry.slug.clone(),
                    name: entry.name,
                    author: entry.author,
                    source: entry.atlas_url,
                    sha256: String::new(),
                    license: entry.license,
                };
                crate::pets_host::pets(&scope).install(&meta, &bytes, false)?;
                Ok::<String, Error>(entry.slug)
            };
            Done::PetInstalled(installed.await)
        });
    }

    /// `Done::PetInstalled`: the gallery pet is in; adopt it.
    pub(super) fn pet_installed(&mut self, installed: Result<String, Error>) {
        match installed {
            Ok(slug) => self.pet_command(&slug),
            Err(err) => self.error(&err),
        }
    }

    /// `/hatch <description>`: the backend is chosen first (none: the
    /// error, nothing spent), then each step asks in the transcript.
    pub(super) fn hatch_command(&mut self, args: &str) {
        if args.trim().is_empty() {
            self.notice("usage: /hatch <description>");
            return;
        }
        let backend = match crate::pets_host::backend::select(self.open.scope) {
            Ok(backend) => backend,
            Err(err) => return self.error(&err),
        };
        let pets = crate::pets_host::pets(self.open.scope);
        let drafts = self.open.scope.dir().join("cache/pets/hatch");
        let (hatch, step) = Hatch::start(args, backend, pets, drafts);
        self.hatched(Box::new(hatch), step);
    }

    /// An answer to `/hatch`'s question; `false` when none is asked.
    pub(super) fn answer_hatch(&mut self, line: &str) -> bool {
        let Some(mut hatch) = self.hatch.take() else {
            return false;
        };
        let line = line.to_owned();
        self.background(async move {
            let step = hatch.answer(&line).await;
            Done::Hatch(hatch, step)
        });
        true
    }

    /// `Done::Hatch`: show the step; keep the flow while it asks.
    pub(super) fn hatched(&mut self, hatch: Box<Hatch>, step: Step) {
        match step {
            Step::Ask(text) => {
                self.notice(text);
                self.hatch = Some(hatch);
            }
            Step::Done { text, failed, .. } if failed => {
                self.app
                    .transcript
                    .push(pmt_tui::transcript::Kind::Error, text);
            }
            Step::Done { text, .. } => self.notice(text),
        }
    }
}
