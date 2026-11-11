//! `prometheus journey` (aliases `learning`, `memory-graph`; spec: memory,
//! Learning journey timeline; Journey pruning and editing): the timeline
//! of learned skills and memory entries, `--play` to watch it build up,
//! `--json` for the nodes, and `list`, `delete`, `edit` to prune it. The
//! same nodes reach daemon clients as `journey.list`; chats run
//! [`slash`].

pub mod nodes;
pub mod prune;
pub mod render;
pub mod slash;

use std::path::Path;
use std::sync::Arc;

use clap::{Args, Subcommand};
use pmt_core::error::Error;
use pmt_store::Store;

use crate::config_cmd::edit::{DEFAULT_EDITOR, launch_editor};
use crate::context::Context;
use crate::output::{json, print_raw};
use crate::prompt;
use nodes::Node;
use prune::Journey;
use render::Style;

/// `prometheus journey`.
#[derive(Debug, Clone, Args)]
pub struct JourneyArgs {
    /// Prune or edit the journey.
    #[command(subcommand)]
    pub command: Option<JourneyCommand>,
    /// Only what was learned since: a duration (`2h`, `3d`, `1w`) or a
    /// date (`2026-11-01`).
    #[arg(long, value_name = "DURATION|DATE", global = true)]
    pub since: Option<String>,
    /// Animate the build-up, one node per frame.
    #[arg(long)]
    pub play: bool,
    /// Frames a second of `--play`.
    #[arg(long, value_name = "N", default_value_t = 8, value_parser = clap::value_parser!(u32).range(1..=60))]
    pub fps: u32,
    /// Columns of the view.
    #[arg(long, value_name = "W", value_parser = clap::value_parser!(u16).range(20..))]
    pub width: Option<u16>,
    /// Rows of the constellation above the timeline (0 for none).
    #[arg(long, value_name = "H")]
    pub height: Option<u16>,
}

/// `prometheus journey <command>`.
#[derive(Debug, Clone, Subcommand)]
pub enum JourneyCommand {
    /// The nodes, one per line: id, kind, origin, created, title.
    List,
    /// Remove a node: a skill is archived (restorable), a memory entry is
    /// removed as a revertible `user` revision.
    Delete {
        /// `skill:<name>` or a memory entry id (`m_...`).
        node: String,
        /// Do not ask (required without a terminal).
        #[arg(long)]
        yes: bool,
    },
    /// Edit a node in `$EDITOR`: a skill's `SKILL.md` or an entry's text,
    /// applied through the usual validation and scan.
    Edit {
        /// `skill:<name>` or a memory entry id (`m_...`).
        node: String,
    },
}

/// Runs `prometheus journey`; `no_color` is the global `--no-color`.
///
/// # Errors
///
/// The command's failure; the caller prints it and exits with its code.
pub fn run(args: &JourneyArgs, ctx: &Context<'_>, no_color: bool) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let store = Arc::new(Store::open(&scope)?);
    let user_home = ctx.launch.user_home().map(std::path::PathBuf::as_path);
    let english = pmt_i18n::Strings::english(pmt_i18n::Surface::Core);
    let journey = Journey {
        scope: &scope,
        store: &store,
        user_home,
        text: &*english,
    };
    match &args.command {
        None => show(&journey, args, ctx, no_color),
        Some(JourneyCommand::List) => list(&journey, args.since.as_deref(), ctx.json),
        Some(JourneyCommand::Delete { node, yes }) => delete(&journey, node, *yes, ctx.json),
        Some(JourneyCommand::Edit { node }) => edit(&journey, node, ctx),
    }
}

fn nodes_of(
    journey: &Journey<'_>,
    since: Option<&str>,
) -> Result<Vec<pmt_rpc::contracts::JourneyNode>, Error> {
    Ok(nodes::list(journey.scope, journey.store, journey.user_home, since)?.nodes)
}

fn show(
    journey: &Journey<'_>,
    args: &JourneyArgs,
    ctx: &Context<'_>,
    no_color: bool,
) -> Result<u8, Error> {
    let nodes = nodes_of(journey, args.since.as_deref())?;
    if ctx.json {
        return json(&nodes);
    }
    let no_color = no_color
        || ctx
            .launch
            .inherited("NO_COLOR")
            .is_some_and(|v| !v.is_empty())
        || !std::io::IsTerminal::is_terminal(&std::io::stdout());
    let style = Style {
        width: args.width.map_or(80, usize::from),
        height: args.height.map_or(10, usize::from),
        color: !no_color,
    };
    if args.play {
        render::play(&nodes, style, args.fps).map_err(|e| {
            Error::new(
                &pmt_core::error::codes::OUTPUT_IO,
                format!("cannot write to stdout: {e}"),
            )
        })?;
        return Ok(0);
    }
    print_raw(&render::view(&nodes, style))
}

fn list(journey: &Journey<'_>, since: Option<&str>, as_json: bool) -> Result<u8, Error> {
    let nodes = nodes_of(journey, since)?;
    if as_json {
        return json(&nodes);
    }
    print_raw(&slash::listing(journey.text, &nodes))
}

fn delete(journey: &Journey<'_>, node: &str, yes: bool, as_json: bool) -> Result<u8, Error> {
    let node = Node::parse(node)?;
    if !yes {
        let what = journey.describe(&node)?;
        if !prompt::stdin_is_tty() {
            eprintln!("error: confirmation needed to {what}; rerun with --yes");
            return Ok(pmt_core::exit::APPROVAL_REQUIRED);
        }
        if !prompt::confirm(&format!("{what}?"))? {
            return Ok(1);
        }
    }
    let done = journey.delete(&node)?;
    if as_json {
        return json(&serde_json::json!({"deleted": done}));
    }
    eprintln!("{done}");
    Ok(0)
}

fn edit(journey: &Journey<'_>, node: &str, ctx: &Context<'_>) -> Result<u8, Error> {
    let node = Node::parse(node)?;
    let original = journey.text(&node)?;
    let draft = journey.scope.dir().join("journey.edit.md");
    let io_error = |e: std::io::Error| {
        Error::new(
            &pmt_core::error::codes::HOME_IO,
            format!("{}: {e}", draft.display()),
        )
    };
    pmt_core::private_fs::write_atomic(&draft, original.as_bytes()).map_err(io_error)?;
    let edited = launch_editor(ctx.launch.editor().unwrap_or(DEFAULT_EDITOR), &draft)
        .and_then(|()| std::fs::read_to_string(&draft).map_err(io_error));
    let _ = std::fs::remove_file(&draft);
    let done = journey.apply(&node, &edited?)?;
    if ctx.json {
        return json(&serde_json::json!({"edited": done}));
    }
    eprintln!("{done}");
    Ok(0)
}

/// The profile's journey for a daemon client (`journey.list`).
///
/// # Errors
///
/// As [`nodes::list`].
pub fn listing(
    scope: &pmt_config::ProfileScope,
    store: &Store,
    user_home: Option<&Path>,
    since: Option<&str>,
) -> Result<pmt_rpc::contracts::JourneyList, Error> {
    nodes::list(scope, store, user_home, since)
}
