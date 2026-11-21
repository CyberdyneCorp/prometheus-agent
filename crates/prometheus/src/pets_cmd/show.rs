//! `pets show` (a preview animated in this terminal) and `pets doctor`.

use std::fmt::Write as _;
use std::io::Write as _;
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_config::schema::PetRenderMode;
use pmt_core::error::{Error, codes};
use pmt_pets::PetState;
use pmt_pets::render::{self, Body, Mode, Renderer, Sprite};
use serde_json::json;

use crate::context::Context;
use crate::pets_host::{current, pets, terminal_env};

/// `pets show` flags.
pub(super) struct Preview {
    pub(super) slug: Option<String>,
    pub(super) state: Option<String>,
    pub(super) cycle: bool,
    pub(super) once: bool,
    pub(super) mode: Option<String>,
    pub(super) scale: Option<f64>,
}

fn usage(message: String) -> Error {
    Error::new(&codes::CLI_USAGE, message)
}

fn states(preview: &Preview) -> Result<Vec<PetState>, Error> {
    if preview.cycle {
        return Ok(PetState::ALL.to_vec());
    }
    match &preview.state {
        None => Ok(vec![PetState::Idle]),
        Some(name) => PetState::parse(name).map(|s| vec![s]).ok_or_else(|| {
            usage(format!(
                "unknown state '{name}' (idle, wave, run, failed, review, jump, waiting)"
            ))
        }),
    }
}

pub(super) fn show(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    preview: &Preview,
) -> Result<u8, Error> {
    let config = current(scope);
    let store = pets(scope);
    let pet = match &preview.slug {
        Some(slug) => store
            .get(slug)
            .ok_or_else(|| pmt_pets::store::not_found(slug))?,
        None => store.active(&config.slug).ok_or_else(|| {
            Error::new(&codes::PET_NOT_FOUND, "no pet is installed")
                .with_hint("install one with 'prometheus pets install <slug>'")
        })?,
    };
    let atlas = pet.load_atlas()?;
    let configured = match &preview.mode {
        Some(name) => PetRenderMode::parse(name).ok_or_else(|| {
            usage(format!(
                "unknown render mode '{name}' (auto, kitty, iterm, sixel, unicode)"
            ))
        })?,
        None => config.render_mode,
    };
    let states = states(preview)?;
    let mode = render::resolve(
        configured,
        crate::prompt::stdout_is_tty(),
        &terminal_env(ctx.launch),
    );
    if mode == Mode::Off {
        eprintln!("nothing drawn: the output is not a terminal or the render mode is off");
        return Ok(0);
    }
    let scale = preview
        .scale
        .map_or(config.scale, |s| pmt_pets::size::clamp_scale(s).0);
    let renderer = Renderer::new(&atlas, &pet.meta.slug, mode, scale, config.unicode_cols);
    super::runtime()?.block_on(animate(&renderer, &pet.meta.slug, &states, preview.once));
    Ok(0)
}

/// The tallest sprite of `states` (the box the preview redraws in).
fn height(renderer: &Renderer, states: &[PetState]) -> u16 {
    states
        .iter()
        .filter_map(|s| renderer.sprite(*s, 0))
        .map(|s| s.rows)
        .max()
        .unwrap_or(1)
}

fn frame_text(sprite: &Sprite, rows: u16, label: &str) -> String {
    // Back to the saved top-left, blank the box, draw, then the label.
    let mut out = String::from("\x1b8\x1b7");
    for _ in 0..=rows {
        out.push_str("\x1b[2K\n");
    }
    out.push_str("\x1b8\x1b7");
    match &sprite.body {
        Body::Graphics(escape) => out.push_str(escape),
        Body::Lines(lines) => {
            for line in lines {
                let _ = writeln!(out, "{line}");
            }
        }
    }
    let _ = write!(out, "\x1b8\x1b[{rows}B\r{label}");
    out
}

/// Plays every frame of `state` once (twice for short rows, so each
/// state shows for a moment); `false` when Ctrl+C stopped it.
async fn play(
    renderer: &Renderer,
    label: &str,
    state: PetState,
    rows: u16,
    signals: &mut crate::chat::signals::Signals,
) -> bool {
    let tick = Duration::from_millis(1000 / u64::from(pmt_pets::frames::FPS));
    let count = renderer.frame_count(state).max(1);
    let plays = if count < 4 { 2 } else { 1 };
    let mut out = std::io::stdout();
    for index in 0..count * plays {
        if let Some(sprite) = renderer.sprite(state, index) {
            let label = format!("{label} · {}", state.as_str());
            let _ = out.write_all(frame_text(&sprite, rows, &label).as_bytes());
            let _ = out.flush();
        }
        tokio::select! {
            () = tokio::time::sleep(tick) => {}
            _ = signals.next() => return false,
        }
    }
    true
}

async fn animate(renderer: &Renderer, slug: &str, states: &[PetState], once: bool) {
    let rows = height(renderer, states);
    let mut out = std::io::stdout();
    // Room for the box and its label, then the box's top-left is saved.
    let room = "\n".repeat(usize::from(rows) + 1);
    let _ = write!(out, "\x1b[?25l{room}\x1b[{}A\r\x1b7", rows + 1);
    let mut signals = crate::chat::signals::Signals::listen();
    'all: loop {
        for &state in states {
            if !play(renderer, slug, state, rows, &mut signals).await {
                break 'all;
            }
        }
        if once {
            break;
        }
    }
    let _ = writeln!(out, "{}\x1b[?25h", renderer.delete());
    let _ = out.flush();
}

/// One doctor finding: the line and whether it blocks.
struct Report {
    lines: Vec<String>,
    missing: Option<String>,
}

impl Report {
    fn line(&mut self, text: String) {
        self.lines.push(text);
    }

    fn need(&mut self, ok: bool, missing: &str) {
        if !ok && self.missing.is_none() {
            self.missing = Some(missing.to_owned());
        }
    }
}

pub(super) fn doctor(ctx: &Context<'_>, scope: &ProfileScope) -> Result<u8, Error> {
    let config = current(scope);
    let store = pets(scope);
    let installed: Vec<String> = store.installed().into_iter().map(|p| p.meta.slug).collect();
    let active = store.active(&config.slug);
    let detected = render::detect(&terminal_env(ctx.launch));
    let effective = render::resolve(
        config.render_mode,
        crate::prompt::stdout_is_tty(),
        &terminal_env(ctx.launch),
    );
    let mut report = Report {
        lines: Vec::new(),
        missing: None,
    };
    report.line(format!("pets directory: {}", store.dir().display()));
    let listed = if installed.is_empty() {
        "none".to_owned()
    } else {
        installed.join(", ")
    };
    report.line(format!("installed: {listed}"));
    report.line(format!("pets.enabled: {}", config.enabled));
    report.line(format!(
        "pets.slug: {}",
        if config.slug.is_empty() {
            "(unset)"
        } else {
            &config.slug
        }
    ));
    report.line(format!(
        "active pet: {}",
        active.as_ref().map_or("none", |p| p.meta.slug.as_str())
    ));
    report.line(format!(
        "render mode: {} (detected: {})",
        config.render_mode.as_str(),
        detected.as_str()
    ));
    report.line(format!("effective mode here: {}", effective.as_str()));
    report.need(!installed.is_empty(), "no pet is installed");
    report.need(
        !config.slug.is_empty(),
        "no pet is selected (pets.slug is empty)",
    );
    report.need(
        config.slug.is_empty() || installed.contains(&config.slug),
        &format!("pets.slug '{}' is not installed", config.slug),
    );
    if let Some(pet) = &active
        && let Err(err) = pet.load_atlas()
    {
        report.need(false, err.message());
    }
    report.need(config.enabled, "pets.enabled is false");
    if ctx.json {
        crate::output::json(&json!({
            "lines": report.lines,
            "ready": report.missing.is_none(),
            "missing": report.missing,
        }))?;
    } else {
        let verdict = report
            .missing
            .as_deref()
            .map_or_else(|| "ready".to_owned(), |m| format!("not ready: {m}"));
        crate::output::print(&format!("{}\n{verdict}", report.lines.join("\n")))?;
    }
    Ok(u8::from(report.missing.is_some()))
}
