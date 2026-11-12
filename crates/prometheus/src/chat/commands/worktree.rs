//! `/worktree [new [NAME] | list | prune [--dry-run]]` and the
//! `worktree.*` RPC methods (spec: git-worktrees, In-session worktree
//! command).
//!
//! `new` creates a tree of the session's repository and moves the session
//! into it ([`Live::retarget`]) without touching its prompt or tools;
//! `/worktree` shows the active tree, `list` the repository's trees and
//! archives, `prune` runs the sweep on the repository.

use std::fmt::Write as _;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_i18n::{Strings, t};
use pmt_rpc::contracts as c;
use pmt_worktrees::create::{self, Request};
use pmt_worktrees::sweep::{self, Action, Mode};
use pmt_worktrees::{Kind, Repo, Tree, archive, registry, tree};

use super::Ctx;
use crate::chat::session::delegation::isolation::settings_of;
use crate::chat::session::{Factory, Live};

/// `/worktree ...`.
///
/// # Errors
///
/// Not a git repository, an invalid or taken name, git failures.
pub async fn worktree(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let mut words = args.split_whitespace();
    match words.next() {
        None | Some("status") => Ok(render_status(&ctx.strings(), &status(ctx.live))),
        Some("new") => {
            let created = new(ctx.factory, ctx.live, ctx.scope, words.next()).await?;
            Ok(render_created(&ctx.strings(), &created))
        }
        Some("list") => list(ctx.live).map(|l| render_listing(&ctx.strings(), &l)),
        Some("prune") => {
            let dry_run = words.any(|w| w == "--dry-run");
            prune(ctx.scope, ctx.live, dry_run).map(|p| render_pruned(&ctx.strings(), &p))
        }
        Some(other) => Err(Error::new(
            &codes::CLI_USAGE,
            format!(
                "unknown /worktree action '{other}': use new [NAME], list or prune [--dry-run]"
            ),
        )),
    }
}

/// Creates a tree of `live`'s repository and moves the session into it.
///
/// # Errors
///
/// `worktree.not_a_repository`, `worktree.invalid_name`, `worktree.git`,
/// environment errors of the move.
pub async fn new(
    factory: &Factory,
    live: &Live,
    scope: &ProfileScope,
    name: Option<&str>,
) -> Result<c::WorktreeCreated, Error> {
    let cwd = live.cwd();
    let request_name = name.map(str::to_owned);
    let sync_remote = scope.config().worktrees.sync_remote;
    let profile = scope.dir().to_path_buf();
    let created = pmt_core::spawn::blocking(move || {
        let request = Request {
            name: request_name.as_deref(),
            kind: Kind::Named,
            sync_remote,
            ..Request::default()
        };
        let created = create::create(&cwd, &request)?;
        if let Err(err) = registry::record(&profile, &created.repo) {
            tracing::warn!(error = %err, "worktree repository not recorded");
        }
        Ok::<_, Error>(created)
    })
    .await
    .map_err(|e| Error::new(&codes::TASK_PANICKED, e.to_string()))??;
    live.retarget(factory, &created.tree.path).await?;
    Ok(c::WorktreeCreated {
        tree: info(&created.tree),
        base: created.base,
        notices: created.notices,
    })
}

/// The session's working directory and the tree it is in.
#[must_use]
pub fn status(live: &Live) -> c::WorktreeStatus {
    let cwd = live.cwd();
    c::WorktreeStatus {
        cwd: cwd.to_string_lossy().into_owned(),
        tree: tree::containing(&cwd).map(|(_, t)| info(&t)),
    }
}

/// The trees and archives of the session's repository.
///
/// # Errors
///
/// `worktree.not_a_repository`, `worktree.git`.
pub fn list(live: &Live) -> Result<c::WorktreeListing, Error> {
    let repo = Repo::discover(&live.cwd())?;
    listing(&repo)
}

/// The trees and archives of `repo`.
///
/// # Errors
///
/// `worktree.git`.
pub fn listing(repo: &Repo) -> Result<c::WorktreeListing, Error> {
    let now = tree::now_secs();
    Ok(c::WorktreeListing {
        repo: repo.root.to_string_lossy().into_owned(),
        trees: tree::list(repo).iter().map(info).collect(),
        archives: archive::list(repo)?
            .into_iter()
            .map(|a| c::WorktreeArchiveInfo {
                name: a.meta.name,
                reference: a.reference,
                branch: a.meta.branch,
                age_secs: now.saturating_sub(a.meta.archived),
                reason: a.meta.reason,
            })
            .collect(),
    })
}

/// Sweeps the session's repository.
///
/// # Errors
///
/// `worktree.not_a_repository`, `worktree.git`.
pub fn prune(scope: &ProfileScope, live: &Live, dry_run: bool) -> Result<c::WorktreePruned, Error> {
    let repo = Repo::discover(&live.cwd())?;
    pruned(&repo, scope, dry_run)
}

/// Sweeps `repo` with `scope`'s settings.
///
/// # Errors
///
/// `worktree.git`.
pub fn pruned(
    repo: &Repo,
    scope: &ProfileScope,
    dry_run: bool,
) -> Result<c::WorktreePruned, Error> {
    let mode = if dry_run { Mode::DryRun } else { Mode::Apply };
    let decisions = sweep::sweep(repo, &settings_of(scope), tree::now_secs(), mode)?;
    Ok(c::WorktreePruned {
        repo: repo.root.to_string_lossy().into_owned(),
        dry_run,
        decisions: decisions.iter().map(decision).collect(),
    })
}

/// The wire form of a sweep decision.
#[must_use]
pub fn decision(d: &sweep::Decision) -> c::WorktreeDecision {
    let (action, reason) = match &d.action {
        Action::Remove => ("remove", None),
        Action::ArchiveRemove => ("archive", None),
        Action::Keep(reason) => ("keep", Some((*reason).to_owned())),
    };
    c::WorktreeDecision {
        name: d.tree.name().to_owned(),
        path: d.tree.path.to_string_lossy().into_owned(),
        action: action.to_owned(),
        reason,
        outcome: d.outcome.clone(),
    }
}

/// The wire form of a tree.
#[must_use]
pub fn info(t: &Tree) -> c::WorktreeInfo {
    c::WorktreeInfo {
        name: t.meta.name.clone(),
        path: t.path.to_string_lossy().into_owned(),
        branch: t.branch(),
        kind: match t.meta.kind {
            Kind::Generated => "generated",
            Kind::Named => "named",
            Kind::Subagent => "subagent",
            Kind::Kanban => "kanban",
        }
        .to_owned(),
        age_secs: tree::now_secs().saturating_sub(t.meta.created),
        last_decision: t.meta.last_decision.clone(),
    }
}

fn render_created(strings: &Strings, created: &c::WorktreeCreated) -> String {
    let mut text = t!(
        strings,
        "slash.worktree.created",
        path = created.tree.path,
        branch = branch(strings, created.tree.branch.as_deref()),
        base = created.base
    );
    for notice in &created.notices {
        let _ = write!(
            text,
            "\n{}",
            t!(strings, "slash.worktree.note", notice = notice)
        );
    }
    text
}

/// A branch name, or `(detached)` in the language of `strings`.
fn branch<'a>(strings: &'a Strings, name: Option<&'a str>) -> &'a str {
    name.unwrap_or_else(|| strings.get("slash.worktree.detached"))
}

fn render_status(strings: &Strings, status: &c::WorktreeStatus) -> String {
    match &status.tree {
        Some(tree) => t!(
            strings,
            "slash.worktree.status",
            name = tree.name,
            branch = branch(strings, tree.branch.as_deref()),
            path = tree.path
        ),
        None => t!(strings, "slash.worktree.none", cwd = status.cwd),
    }
}

/// The text of a listing (also `prometheus worktrees list`).
#[must_use]
pub fn render_listing(strings: &Strings, listing: &c::WorktreeListing) -> String {
    let mut text = format!("{}:", listing.repo);
    if listing.trees.is_empty() && listing.archives.is_empty() {
        let _ = write!(text, " {}", strings.get("slash.worktree.empty"));
    }
    for tree in &listing.trees {
        let _ = write!(
            text,
            "\n  {}  {}  {}  {}",
            tree.name,
            branch(strings, tree.branch.as_deref()),
            age(tree.age_secs),
            tree.last_decision.as_deref().unwrap_or("-")
        );
    }
    for a in &listing.archives {
        let archived = t!(strings, "slash.worktree.archived", reason = a.reason);
        let _ = write!(
            text,
            "\n  {}  {}  {}  {archived}",
            a.name,
            branch(strings, a.branch.as_deref()),
            age(a.age_secs),
        );
    }
    text
}

/// The text of a sweep (also `prometheus worktrees prune`).
#[must_use]
pub fn render_pruned(strings: &Strings, pruned: &c::WorktreePruned) -> String {
    let mut text = pruned.repo.clone();
    if pruned.dry_run {
        let _ = write!(text, " {}", strings.get("slash.worktree.dry_run"));
    }
    text.push(':');
    if pruned.decisions.is_empty() {
        let _ = write!(text, " {}", strings.get("slash.worktree.no_worktrees"));
    }
    for d in &pruned.decisions {
        let _ = write!(text, "\n  {}  {}", d.name, d.outcome);
    }
    text
}

fn age(secs: u64) -> String {
    match secs {
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}
