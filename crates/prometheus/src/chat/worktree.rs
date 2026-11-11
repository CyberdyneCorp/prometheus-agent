//! `prometheus chat -w/--worktree [NAME]` (spec: git-worktrees, chat -w
//! worktree mode; Exit disposition; Repository registry and sweep
//! schedule).
//!
//! Before anything starts, the repository of the launch directory is
//! recorded and every recorded repository swept; then a tree is created
//! (or, for a resumed session that already owns one, reopened, restored
//! from its archive when the sweep took it) and the whole session runs in
//! it as `--cwd`. When an interactive session ends, a tree with nothing to
//! lose is removed and one with unfinished work is kept unless the person
//! explicitly chooses otherwise; nothing asks without a terminal.

use std::io::{BufRead as _, Write as _};
use std::path::Path;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_worktrees::create::{self, Request};
use pmt_worktrees::exit::{self, Choice, Disposition};
use pmt_worktrees::sweep::Mode;
use pmt_worktrees::{Kind, Repo, Tree, archive, registry, tree};

use super::Mode as ChatMode;
use super::args::ChatArgs;
use crate::chat::session::delegation::isolation::settings_of;

/// A chat running in a tree.
pub struct Entered {
    /// The repository.
    pub repo: Repo,
    /// The tree.
    pub tree: Tree,
    /// The chat's flags with `--cwd` set to the tree.
    pub args: ChatArgs,
}

/// Enters the tree `-w` asks for, changing the process's directory to it.
///
/// # Errors
///
/// `--worktree requires a git repository` (exit 2), an invalid or taken
/// name, git and store failures.
pub fn enter(args: &ChatArgs, scope: &ProfileScope, quiet: bool) -> Result<Option<Entered>, Error> {
    let Some(name) = &args.worktree else {
        return Ok(None);
    };
    let cwd = std::env::current_dir().map_err(|e| {
        Error::new(
            &codes::CHAT_CWD_MISSING,
            format!("no working directory: {e}"),
        )
    })?;
    let repo = Repo::discover(&cwd)
        .map_err(|_| Error::new(&codes::CLI_USAGE, "--worktree requires a git repository"))?;
    if let Err(err) = registry::record(scope.dir(), &repo) {
        tracing::warn!(error = %err, "worktree repository not recorded");
    }
    sweep_recorded(scope);
    let tree = if let Some(tree) = owned_by_resumed(args, scope, &repo, &cwd)? {
        tree
    } else {
        let request = Request {
            name: Some(name.as_str()).filter(|n| !n.is_empty()),
            kind: Kind::Named,
            sync_remote: scope.config().worktrees.sync_remote,
            ..Request::default()
        };
        let created = create::create(&cwd, &request)?;
        if !quiet {
            let strings = strings(scope);
            for notice in &created.notices {
                eprintln!(
                    "{}",
                    pmt_i18n::t!(strings, "cli.worktree_note", notice = notice)
                );
            }
        }
        created.tree
    };
    std::env::set_current_dir(&tree.path).map_err(|e| {
        Error::new(
            &codes::CHAT_CWD_MISSING,
            format!(
                "cannot use {} as the working directory: {e}",
                tree.path.display()
            ),
        )
    })?;
    let mut args = args.clone();
    args.cwd = Some(tree.path.clone());
    Ok(Some(Entered { repo, tree, args }))
}

fn strings(scope: &ProfileScope) -> std::sync::Arc<pmt_i18n::Strings> {
    pmt_config::language::strings(scope.dir(), pmt_i18n::Surface::Core)
}

/// The sweep every `chat -w` start runs over the recorded repositories.
fn sweep_recorded(scope: &ProfileScope) {
    let swept = registry::sweep_all(
        scope.dir(),
        &settings_of(scope),
        tree::now_secs(),
        Mode::Apply,
    );
    for repo in swept {
        if let Err(err) = repo.decisions {
            tracing::warn!(repo = %repo.repo.root.display(), error = %err, "worktree sweep failed");
        }
    }
}

/// The tree of the session `-r`/`-c` resumes, when it owns one of `repo`
/// (restored from its archive when the sweep took it).
fn owned_by_resumed(
    args: &ChatArgs,
    scope: &ProfileScope,
    repo: &Repo,
    cwd: &Path,
) -> Result<Option<Tree>, Error> {
    let request = super::resume::Request::of(args.resume.as_deref(), args.continue_.as_deref());
    if request == super::resume::Request::New {
        return Ok(None);
    }
    let store = pmt_store::Store::open(scope)?;
    let place = super::resume::Place {
        cwd,
        terminal: None,
    };
    let Some(id) = super::resume::resolve(&store, &request, place)? else {
        return Ok(None);
    };
    let Some(recorded) = store.session(id)?.cwd.map(std::path::PathBuf::from) else {
        return Ok(None);
    };
    if let Some(tree) = tree::list(repo)
        .into_iter()
        .find(|t| recorded.starts_with(&t.path))
    {
        return Ok(Some(tree));
    }
    let Some(name) = recorded
        .strip_prefix(repo.trees_dir())
        .ok()
        .and_then(|rest| rest.components().next())
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
    else {
        return Ok(None);
    };
    if archive::find(repo, &name).is_err() {
        return Ok(None);
    }
    archive::restore(repo, &name, None)?;
    Ok(tree::find(repo, &name))
}

/// What the end of the session does to its tree: an interactive session
/// at a terminal removes a tree with nothing to lose and asks about one
/// with unfinished work; every other run keeps it for the sweep.
pub fn finish(entered: &Entered, mode: ChatMode, scope: &ProfileScope, quiet: bool) {
    let interactive = mode != ChatMode::OneShot && super::terminal::interactive();
    let strings = strings(scope);
    let path = entered.tree.path.display();
    // The process leaves the tree before anything may remove it.
    let _ = std::env::set_current_dir(&entered.repo.root);
    if !interactive {
        if !quiet {
            eprintln!(
                "{}",
                pmt_i18n::t!(strings, "cli.worktree_kept", path = path)
            );
        }
        return;
    }
    let choice = match exit::assess(&entered.repo, &entered.tree) {
        Disposition::Remove => Choice::RemoveDiscard,
        Disposition::Unknown => Choice::Keep,
        Disposition::Unfinished { dirty, unpushed } => {
            ask(&strings, &entered.tree, dirty, unpushed)
        }
    };
    match exit::apply(&entered.repo, &entered.tree, choice, &settings_of(scope)) {
        Ok(text) => println!("{text}"),
        Err(err) => eprintln!(
            "{}",
            pmt_i18n::t!(
                strings,
                "cli.worktree_kept_error",
                why = err.message(),
                path = path
            )
        ),
    }
}

/// Asks what to do with unfinished work; Enter, Esc, end of input and
/// anything unrecognized keep the tree.
fn ask(strings: &pmt_i18n::Strings, tree: &Tree, dirty: bool, unpushed: u32) -> Choice {
    let what = match (dirty, unpushed) {
        (true, 0) => pmt_i18n::t!(strings, "cli.worktree_dirty"),
        (false, n) => pmt_i18n::t!(strings, "cli.worktree_unpushed", count = n),
        (true, n) => pmt_i18n::t!(strings, "cli.worktree_dirty_unpushed", count = n),
    };
    let question = pmt_i18n::t!(
        strings,
        "cli.worktree_question",
        path = tree.path.display(),
        what = what
    );
    print!("{question}");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    match std::io::stdin().lock().read_line(&mut answer) {
        Ok(0) | Err(_) => Choice::Keep,
        Ok(_) => Choice::parse(&answer),
    }
}
