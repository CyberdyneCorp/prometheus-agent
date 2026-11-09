//! `prometheus replay export|run|bless` (spec: replay-and-evaluation,
//! Session recordings; Replay modes; Divergence detection; Golden
//! recording regression gate).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use pmt_core::error::{Error, codes};
use pmt_core::exit;
use pmt_eval::recording::{Recording, member};

use crate::context::Context;
use crate::output::print_raw;
use crate::replay::run::{Report, Settings};
use crate::replay::{Mode, OnDivergence, State, export};

/// `prometheus replay` subcommands.
#[derive(Debug, Subcommand)]
pub enum ReplayCommand {
    /// Write a session's recording (`.pmtrec`).
    Export {
        /// The session (id or unique title).
        session: String,
        /// The file (default `<session>.pmtrec`).
        #[arg(short = 'o', long, value_name = "FILE")]
        output: Option<PathBuf>,
        /// Include the workspace files' contents (only their manifest
        /// otherwise).
        #[arg(long)]
        include_workspace: bool,
        /// Include every descendant session (subagents, forks).
        #[arg(long)]
        lineage: bool,
    },
    /// Re-execute a recording and report the first divergence.
    Run {
        /// The `.pmtrec` file.
        recording: PathBuf,
        /// What is served from the recording.
        #[arg(long, value_enum, default_value_t = Mode::Strict)]
        mode: Mode,
        /// Stop at the first divergence, or continue live.
        #[arg(long, value_enum, default_value_t = OnDivergence::Stop)]
        on_divergence: OnDivergence,
        /// Where the replay's home, workspace and trajectory go (default:
        /// a new directory under the profile's `cache/replay/`).
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
    },
    /// Re-record a recording with this build and print the diff accepted.
    Bless {
        /// The `.pmtrec` file (rewritten in place).
        recording: PathBuf,
        /// Only `strict` re-records.
        #[arg(long, value_enum, default_value_t = Mode::Strict)]
        mode: Mode,
    },
}

/// Runs `prometheus replay`.
///
/// # Errors
///
/// `replay.*` errors; a divergence exits 1.
pub fn run(command: &ReplayCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        ReplayCommand::Export {
            session,
            output,
            include_workspace,
            lineage,
        } => export_cmd(
            ctx,
            session,
            output.as_deref(),
            export::Options {
                include_workspace: *include_workspace,
                lineage: *lineage,
            },
        ),
        ReplayCommand::Run {
            recording,
            mode,
            on_divergence,
            out,
        } => run_cmd(ctx, recording, (*mode, *on_divergence), out.as_deref()),
        ReplayCommand::Bless { recording, mode } => bless(ctx, recording, *mode),
    }
}

fn export_cmd(
    ctx: &Context<'_>,
    session: &str,
    output: Option<&Path>,
    options: export::Options,
) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let store = pmt_store::Store::open(&scope)?;
    let id = store.resolve_session(session)?;
    let mut recording = export::recording(&scope, &store, id, options)?;
    let path = output.map_or_else(
        || PathBuf::from(format!("{id}.{}", pmt_eval::recording::EXTENSION)),
        Path::to_path_buf,
    );
    recording.write(&path)?;
    print_raw(&format!(
        "wrote {} ({} sessions)\n",
        path.display(),
        recording.manifest.session_ids.len()
    ))
}

/// A new directory under the profile's `cache/replay/`.
fn default_out(ctx: &Context<'_>) -> Result<PathBuf, Error> {
    let scope = ctx.scope()?;
    let dir = scope
        .path("cache/replay")
        .join(ulid::Ulid::generate().to_string().to_lowercase());
    pmt_core::private_fs::create_dir(&dir).map_err(|e| Error::io(&codes::REPLAY_IO, &dir, &e))?;
    Ok(dir)
}

fn replay(
    ctx: &Context<'_>,
    recording: &Recording,
    (mode, on_divergence): (Mode, OnDivergence),
    out: &Path,
) -> Result<Report, Error> {
    let scope = ctx.scope().ok();
    crate::replay::run::run(
        recording,
        &Settings {
            mode,
            on_divergence,
            out,
            launch: ctx.launch,
            secrets_from: scope.as_ref(),
        },
    )
}

fn run_cmd(
    ctx: &Context<'_>,
    path: &Path,
    modes: (Mode, OnDivergence),
    out: Option<&Path>,
) -> Result<u8, Error> {
    let recording = Recording::open(path)?;
    let out = match out {
        Some(dir) => {
            pmt_core::private_fs::create_dir(dir)
                .map_err(|e| Error::io(&codes::REPLAY_IO, dir, &e))?;
            dir.to_path_buf()
        }
        None => default_out(ctx)?,
    };
    let report = replay(ctx, &recording, modes, &out)?;
    let mut text = report.text();
    if let Some(trajectory) = &report.trajectory {
        let _ = writeln!(text, "trajectory: {}", trajectory.display());
    }
    print_raw(&text)?;
    Ok(if report.matched() {
        exit::SUCCESS
    } else {
        exit::FAILURE
    })
}

/// Re-records `path`: a strict replay that continues live from the first
/// divergence (the recorded mock script answers, tools run in the
/// recorded workspace), exported again with the original workspace.
fn bless(ctx: &Context<'_>, path: &Path, mode: Mode) -> Result<u8, Error> {
    if mode != Mode::Strict {
        return Err(Error::new(
            &codes::CLI_USAGE,
            "replay bless re-records in strict mode only",
        ));
    }
    let old = Recording::open(path)?;
    let state: State = old.json(member::STATE)?.unwrap_or_default();
    let out = default_out(ctx)?;
    let report = replay(ctx, &old, (Mode::Strict, OnDivergence::Continue), &out)?;
    if report.matched() {
        let _ = std::fs::remove_dir_all(&out);
        return print_raw(&format!("{}: unchanged (replay matched)\n", path.display()));
    }
    let mut new = rerecorded(&old, &state, &out)?;
    new.write(path)?;
    let _ = std::fs::remove_dir_all(&out);
    print_raw(&format!("{}blessed {}\n", report.text(), path.display()))
}

/// The replay's session in `out`, exported with `old`'s workspace.
fn rerecorded(old: &Recording, state: &State, out: &Path) -> Result<Recording, Error> {
    let home = out.join("home");
    let launch = pmt_core::env::LaunchEnv::default();
    let ctx = Context {
        home_flag: Some(&home),
        launch: &launch,
        profile: None,
        set: &[],
        json: false,
    };
    let scope = ctx.scope()?;
    let store = pmt_store::Store::open(&scope)?;
    let root = pmt_core::id::SessionId::parse(old.root().unwrap_or_default())?;
    let options = export::Options {
        include_workspace: false,
        lineage: old.manifest.session_ids.len() > 1,
    };
    let mut new = export::recording(&scope, &store, root, options)?;
    let mut new_state: State = new.json(member::STATE)?.unwrap_or_default();
    new_state.cwd.clone_from(&state.cwd);
    new_state.workspace_included = state.workspace_included;
    new.add_json(member::STATE, &new_state);
    for (name, bytes) in old.members() {
        if name.starts_with("workspace/") {
            new.add(name, bytes.to_vec());
        }
    }
    Ok(new)
}
