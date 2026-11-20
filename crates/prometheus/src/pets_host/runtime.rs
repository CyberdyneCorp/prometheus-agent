//! A terminal surface's pet: the session's activity ([`Tracker`]), the
//! renderer of the pet shown and the frame counter. The renderer follows
//! `config.toml`: when the file changes (a `/pet`, `prometheus pets`, the
//! desktop's scale) the `[pets]` keys are read again within a second.

use std::time::{Duration, Instant, SystemTime};

use pmt_config::ProfileScope;
use pmt_pets::render::{Renderer, Sprite, unicode};
use pmt_pets::state::Tracker;
use serde_json::Value;

/// How often `config.toml` is checked for `[pets]` changes.
const RECHECK: Duration = Duration::from_secs(1);

/// One surface's pet.
pub struct Runtime {
    scope: ProfileScope,
    env: pmt_core::env::LaunchEnv,
    /// What the session is doing.
    pub tracker: Tracker,
    renderer: Option<Renderer>,
    frame: usize,
    stamp: Option<SystemTime>,
    checked: Option<Instant>,
}

impl Runtime {
    /// The pet of a surface writing to a terminal.
    #[must_use]
    pub fn new(scope: &ProfileScope, env: &pmt_core::env::LaunchEnv) -> Self {
        let mut runtime = Self {
            scope: scope.clone(),
            env: env.clone(),
            tracker: Tracker::default(),
            renderer: None,
            frame: 0,
            stamp: None,
            checked: None,
        };
        runtime.refresh(true, Instant::now());
        runtime
    }

    fn config_stamp(&self) -> Option<SystemTime> {
        std::fs::metadata(self.scope.profile().config_file().path())
            .and_then(|m| m.modified())
            .ok()
    }

    /// Rebuilds the renderer when `config.toml` changed (or `force`).
    pub fn refresh(&mut self, force: bool, now: Instant) {
        if !force
            && self
                .checked
                .is_some_and(|at| now.duration_since(at) < RECHECK)
        {
            return;
        }
        self.checked = Some(now);
        let stamp = self.config_stamp();
        if !force && stamp == self.stamp {
            return;
        }
        self.stamp = stamp;
        let is_tty = crate::prompt::stdout_is_tty();
        self.renderer = super::renderer(&self.scope, &self.env, is_tty).map(|(_, r)| r);
    }

    /// The renderer, when a pet shows.
    #[must_use]
    pub fn renderer(&self) -> Option<&Renderer> {
        self.renderer.as_ref()
    }

    /// Advances one frame (and re-reads the configuration when due).
    pub fn tick(&mut self, now: Instant) {
        self.refresh(false, now);
        self.frame = self.frame.wrapping_add(1);
    }

    /// A trajectory event of the session.
    pub fn event(&mut self, kind: &str, payload: &Value) {
        self.tracker.event(kind, payload, Instant::now());
    }

    /// The user sent `text`: an affectionate one plays the heart.
    pub fn sent(&mut self, text: &str) {
        if pmt_pets::affection::is_affectionate(text) {
            self.tracker.heart(Instant::now());
        }
    }

    /// This frame, encoded for the terminal.
    #[must_use]
    pub fn sprite(&self, now: Instant) -> Option<Sprite> {
        self.renderer
            .as_ref()?
            .sprite(self.tracker.state(now), self.frame)
    }

    /// This frame as half-block cells (empty for a graphics pet).
    #[must_use]
    pub fn cells(&self, now: Instant) -> Vec<Vec<unicode::Cell>> {
        self.renderer
            .as_ref()
            .map(|r| r.cells(self.tracker.state(now), self.frame))
            .unwrap_or_default()
    }

    /// Whether the heart shows.
    #[must_use]
    pub fn heart(&self, now: Instant) -> bool {
        self.tracker.hearts(now)
    }

    /// What removes a graphics pet from the terminal.
    #[must_use]
    pub fn delete(&self) -> String {
        self.renderer
            .as_ref()
            .map(Renderer::delete)
            .unwrap_or_default()
    }
}
