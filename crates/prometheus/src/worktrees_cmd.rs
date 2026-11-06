//! `prometheus worktrees list|prune|restore` (spec: git-worktrees,
//! Worktree CLI and restore).
//!
//! The commands act on the repositories the profile recorded (and on the
//! repository of the current directory): `list` shows live trees and
//! archives, `prune` runs the sweep (`--dry-run` prints every decision
//! and changes nothing), `restore` recreates an archived tree.

use std::path::{Path, PathBuf};

use clap::Subcommand;
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_worktrees::{Repo, archive, registry};
use serde::Serialize;

use crate::chat::commands::worktree::{listing, pruned, render_listing, render_pruned};
use crate::context::Context;
use crate::output::{document, print_raw};

/// `prometheus worktrees` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum WorktreesCommand {
    /// Live trees and archives: name, age, branch and last decision.
    List {
        /// Only this repository (default: every recorded one and the
        /// current directory's).
        #[arg(long, value_name = "PATH")]
        repo: Option<PathBuf>,
    },
    /// Run the sweep: remove idle clean trees, archive then remove idle
    /// unfinished ones, keep the rest with a reason.
    Prune {
        /// Print every decision with its reason; change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Only this repository (default: every recorded one).
        #[arg(long, value_name = "PATH")]
        repo: Option<PathBuf>,
    },
    /// Recreate an archived tree with its branch, staged and untracked
    /// files; exits 66 when no archive has that name.
    Restore {
        /// The archived tree's name.
        name: String,
        /// Where to recreate it (default `<repo>/.worktrees/<name>`).
        #[arg(long, value_name = "DIR")]
        path: Option<PathBuf>,
    },
}

/// Runs one `worktrees` subcommand.
///
/// # Errors
///
/// The profile cannot be opened, `--repo` is not a repository (2), an
/// unknown archive (66), git failures (74).
pub fn run(command: &WorktreesCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    match command {
        WorktreesCommand::List { repo } => list(&scope, repo.as_deref(), ctx.json),
        WorktreesCommand::Prune { dry_run, repo } => {
            prune(&scope, repo.as_deref(), *dry_run, ctx.json)
        }
        WorktreesCommand::Restore { name, path } => {
            restore(&scope, name, path.as_deref(), ctx.json)
        }
    }
}

/// The repositories a command covers: `--repo`, else the recorded ones
/// (plus, with `with_cwd`, the current directory's).
fn repos(scope: &ProfileScope, only: Option<&Path>, with_cwd: bool) -> Result<Vec<Repo>, Error> {
    if let Some(path) = only {
        return Ok(vec![Repo::discover(path)?]);
    }
    let mut repos = registry::repos(scope.dir());
    if with_cwd
        && let Ok(cwd) = std::env::current_dir()
        && let Ok(here) = Repo::discover(&cwd)
        && !repos.contains(&here)
    {
        repos.insert(0, here);
    }
    Ok(repos)
}

#[derive(Serialize)]
struct Listed {
    repos: Vec<pmt_rpc::contracts::WorktreeListing>,
}

fn list(scope: &ProfileScope, only: Option<&Path>, json: bool) -> Result<u8, Error> {
    let listed = repos(scope, only, true)?
        .iter()
        .map(listing)
        .collect::<Result<Vec<_>, _>>()?;
    if json {
        return document("worktrees list", &Listed { repos: listed });
    }
    if listed.is_empty() {
        return print_raw("no recorded repositories\n");
    }
    let text: Vec<String> = listed.iter().map(render_listing).collect();
    print_raw(&format!("{}\n", text.join("\n")))
}

#[derive(Serialize)]
struct Pruned {
    repos: Vec<pmt_rpc::contracts::WorktreePruned>,
}

fn prune(
    scope: &ProfileScope,
    only: Option<&Path>,
    dry_run: bool,
    json: bool,
) -> Result<u8, Error> {
    let swept = repos(scope, only, false)?
        .iter()
        .map(|repo| pruned(repo, scope, dry_run))
        .collect::<Result<Vec<_>, _>>()?;
    if json {
        return document("worktrees prune", &Pruned { repos: swept });
    }
    if swept.is_empty() {
        return print_raw("no recorded repositories\n");
    }
    let text: Vec<String> = swept.iter().map(render_pruned).collect();
    print_raw(&format!("{}\n", text.join("\n")))
}

#[derive(Serialize)]
struct Restored {
    name: String,
    path: String,
    branch: Option<String>,
}

fn restore(scope: &ProfileScope, name: &str, path: Option<&Path>, json: bool) -> Result<u8, Error> {
    let repo = repos(scope, None, true)?
        .into_iter()
        .find(|repo| archive::find(repo, name).is_ok())
        .ok_or_else(|| {
            Error::new(
                &codes::WORKTREE_ARCHIVE_NOT_FOUND,
                format!("no archive named '{name}' in the current or any recorded repository"),
            )
        })?;
    let restored = archive::restore(&repo, name, path)?;
    if let Err(err) = registry::record(scope.dir(), &repo) {
        tracing::warn!(error = %err, "worktree repository not recorded");
    }
    let body = Restored {
        name: name.to_owned(),
        path: restored.path.to_string_lossy().into_owned(),
        branch: restored.branch,
    };
    if json {
        return document("worktrees restore", &body);
    }
    print_raw(&format!(
        "restored {} at {} ({})\n",
        body.name,
        body.path,
        body.branch.as_deref().map_or_else(
            || "detached at the archived HEAD".to_owned(),
            |b| format!("branch {b}")
        )
    ))
}
