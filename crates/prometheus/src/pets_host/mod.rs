//! Pets on this machine's surfaces (spec: pets): what the `pets` commands,
//! `/pet` and `/hatch` on the line interface and the TUI, the terminal
//! pet and the daemon's `pets.*` and `achievements.*` methods share.
//!
//! Every change goes through a validated `config.toml` patch of the
//! `[pets]` keys, which apply live and never reach a session (the prompt,
//! tools and memory of running sessions stay as they are).
//!
//! - [`slash`]: `/pet` on every terminal surface;
//! - [`runtime`]: a terminal surface's pet (activity, renderer, frame);
//! - [`line`]: the line interface's pet pane;
//! - [`backend`]: `/hatch`'s image backend over the profile's image
//!   provider;
//! - [`achievements`]: the zone and the scan after each turn.

pub mod achievements;
pub mod backend;
pub mod line;
pub mod rpc;
pub mod runtime;
pub mod slash;

use pmt_config::ProfileScope;
use pmt_config::schema::PetsConfig;
use pmt_core::error::Error;
use pmt_pets::render::{Mode, Renderer, TerminalEnv};
use pmt_pets::{Atlas, Installed, Pets};

/// The pets of `scope`'s profile.
#[must_use]
pub fn pets(scope: &ProfileScope) -> Pets {
    Pets::new(scope.dir())
}

/// Writes `changes` (key, value) to the profile's `config.toml` in one
/// validated patch.
///
/// # Errors
///
/// Configuration write and validation errors.
pub fn patch(scope: &ProfileScope, changes: &[(&str, String)]) -> Result<(), Error> {
    let changes: Vec<(String, Option<String>)> = changes
        .iter()
        .map(|(k, v)| ((*k).to_owned(), Some(v.clone())))
        .collect();
    scope.profile().config_file().patch(&changes)
}

/// The `[pets]` section as `config.toml` holds it now: the keys apply
/// live, so long-running surfaces read them again instead of keeping the
/// snapshot they started with (falls back to that snapshot when the file
/// does not read).
#[must_use]
pub fn current(scope: &ProfileScope) -> PetsConfig {
    let path = scope.profile().config_file().path().to_path_buf();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .map(|table| table.get("pets").cloned())
        .map_or_else(
            || scope.config().pets.clone(),
            |pets| {
                pets.map_or_else(PetsConfig::default, |value| {
                    value
                        .try_into()
                        .unwrap_or_else(|_| scope.config().pets.clone())
                })
            },
        )
}

/// The pet shown when pets are on: the configured one if installed, else
/// the first installed, with its validated atlas. `None` when pets are
/// off or nothing usable is installed.
#[must_use]
pub fn shown(scope: &ProfileScope) -> Option<(Installed, Atlas)> {
    let config = current(scope);
    if !config.enabled {
        return None;
    }
    let pet = pets(scope).active(&config.slug)?;
    match pet.load_atlas() {
        Ok(atlas) => Some((pet, atlas)),
        Err(err) => {
            tracing::warn!(pet = pet.meta.slug, "pet not shown: {}", err.message());
            None
        }
    }
}

/// The terminal variables detection reads, from the launch environment.
#[must_use]
pub fn terminal_env(env: &pmt_core::env::LaunchEnv) -> TerminalEnv {
    TerminalEnv::from_lookup(|name| {
        env.inherited(name)
            .map(|v| v.to_string_lossy().into_owned())
    })
}

/// A renderer of the shown pet for a terminal (`is_tty`: the output is
/// one); `None` when nothing is drawn there.
#[must_use]
pub fn renderer(
    scope: &ProfileScope,
    env: &pmt_core::env::LaunchEnv,
    is_tty: bool,
) -> Option<(Installed, Renderer)> {
    let (pet, atlas) = shown(scope)?;
    let config = current(scope);
    let mode = pmt_pets::render::resolve(config.render_mode, is_tty, &terminal_env(env));
    if mode == Mode::Off {
        return None;
    }
    let renderer = Renderer::new(
        &atlas,
        &pet.meta.slug,
        mode,
        config.scale,
        config.unicode_cols,
    );
    Some((pet, renderer))
}
