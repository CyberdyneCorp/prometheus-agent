//! `prometheus pets` (spec: pets, Pet gallery commands; Pet configuration
//! and scale; Pet preview and doctor; Generating a pet): the gallery, the
//! installed pets, which one shows and how big, a terminal preview, the
//! doctor and `hatch`. Every change is a validated patch of `[pets]`.

mod gallery;
mod hatch;
mod select;
mod show;

use clap::Subcommand;
use pmt_core::error::Error;

use crate::context::Context;

/// `prometheus pets` subcommands.
#[derive(Debug, Subcommand)]
pub enum PetsCommand {
    /// List gallery pets (`pets.gallery_url`) whose slug or name contains
    /// QUERY, or the installed ones with `--installed`.
    #[command(visible_alias = "ls")]
    List {
        /// Filter by slug or name.
        query: Option<String>,
        /// At most this many.
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// The installed pets instead of the gallery.
        #[arg(long)]
        installed: bool,
    },
    /// Download a gallery pet over HTTPS, validate its atlas and install
    /// it under `<profile>/pets/<slug>/`.
    Install {
        /// The gallery slug.
        slug: String,
        /// Show it from now on (`pets.slug`, `pets.enabled = true`).
        #[arg(long)]
        select: bool,
        /// Replace an installed pet of that slug.
        #[arg(long)]
        force: bool,
    },
    /// Show an installed pet (a picker on a terminal when SLUG is
    /// omitted).
    Select {
        /// The installed pet.
        slug: Option<String>,
    },
    /// Hide the pet (`pets.enabled = false`).
    Off,
    /// Delete an installed pet (hiding it when it was the one shown).
    Remove {
        /// The installed pet.
        slug: String,
    },
    /// Set the size on every surface (0.1-3.0; out-of-range values are
    /// clamped).
    Scale {
        /// The factor of the native 192x208 cell.
        #[arg(allow_hyphen_values = true)]
        factor: String,
    },
    /// Animate a pet in this terminal until Ctrl+C.
    Show {
        /// The installed pet (default: the active one).
        slug: Option<String>,
        /// The animation state (idle, wave, run, failed, review, jump,
        /// waiting).
        #[arg(long, value_name = "STATE")]
        state: Option<String>,
        /// Go through every state in turn.
        #[arg(long)]
        cycle: bool,
        /// Play the animation once and exit.
        #[arg(long)]
        once: bool,
        /// The render mode (auto, kitty, iterm, sixel, unicode).
        #[arg(long, value_name = "MODE")]
        mode: Option<String>,
        /// The scale (default `pets.scale`).
        #[arg(long, value_name = "FACTOR", allow_hyphen_values = true)]
        scale: Option<f64>,
    },
    /// Check what showing a pet needs; prints `ready` and exits 0, else
    /// exits 1 naming the first missing item.
    Doctor,
    /// Generate a pet from a description with the profile's image backend
    /// (asks before each paid step).
    Hatch {
        /// What the pet looks like.
        #[arg(required = true, num_args = 1..)]
        description: Vec<String>,
    },
}

/// Runs `prometheus pets`.
///
/// # Errors
///
/// `pet.not_found` (66), `pet.invalid_atlas` (65), `pet.exists` (75),
/// `pet.gallery` (69), `pet.no_image_backend` (78), configuration errors.
pub fn run(command: &PetsCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    match command {
        PetsCommand::List {
            query,
            limit,
            installed,
        } => gallery::list(
            ctx,
            &scope,
            query.as_deref().unwrap_or(""),
            *limit,
            *installed,
        ),
        PetsCommand::Install {
            slug,
            select,
            force,
        } => gallery::install(ctx, &scope, slug, *select, *force),
        PetsCommand::Select { slug } => select::select(&scope, slug.as_deref()),
        PetsCommand::Off => select::off(&scope),
        PetsCommand::Remove { slug } => select::remove(&scope, slug),
        PetsCommand::Scale { factor } => {
            crate::output::print(&crate::pets_host::slash::set_scale(&scope, factor)?)
        }
        PetsCommand::Show {
            slug,
            state,
            cycle,
            once,
            mode,
            scale,
        } => show::show(
            ctx,
            &scope,
            &show::Preview {
                slug: slug.clone(),
                state: state.clone(),
                cycle: *cycle,
                once: *once,
                mode: mode.clone(),
                scale: *scale,
            },
        ),
        PetsCommand::Doctor => show::doctor(ctx, &scope),
        PetsCommand::Hatch { description } => hatch::run(ctx, &scope, &description.join(" ")),
    }
}

/// An HTTP client with the profile's network settings.
fn client(ctx: &Context<'_>) -> Result<reqwest::Client, Error> {
    pmt_net::client(&crate::update_cmd::outbound_policy(ctx))
}

/// A runtime for one command's network calls.
fn runtime() -> Result<tokio::runtime::Runtime, Error> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            Error::new(
                &pmt_core::error::codes::OUTPUT_IO,
                format!("cannot start the runtime: {e}"),
            )
        })
}
