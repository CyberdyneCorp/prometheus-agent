//! `/plan` (spec: cli, Planning-only turn) and the profile archive slash
//! forms `/export` and `/import` (spec: cli, Exit with delete and profile
//! archive slash forms).

use std::path::{Path, PathBuf};

use pmt_agent::turn::TurnInput;
use pmt_core::error::{Error, codes};

use super::{Ctx, Reply};
use crate::chat::session::plan::{self, Plan};

/// `/plan [task]`: arms the planning restriction for the next turn and
/// sends the plan request as that turn's message. Without a task the
/// conversation so far is the task (and the session title, if any, names
/// the file).
pub(super) fn plan(ctx: &Ctx<'_>, args: &str) -> Reply {
    let task = args.trim();
    let name = if task.is_empty() {
        ctx.factory
            .store
            .session(ctx.live.id)
            .ok()
            .and_then(|s| s.title)
            .unwrap_or_default()
    } else {
        task.to_owned()
    };
    let plan = Plan::new(&ctx.live.place.cwd(), &name);
    let request = plan::request(&plan, task);
    ctx.live.tools.begin_plan(plan);
    Reply::Send(TurnInput::text(request))
}

/// The line shown after a planning turn: where the plan is, or that none
/// was written.
#[must_use]
pub fn finished(strings: &pmt_i18n::Strings, plan: &Plan) -> String {
    if plan.file.is_file() {
        pmt_i18n::t!(strings, "slash.plan.written", path = plan.file.display())
    } else {
        pmt_i18n::t!(strings, "slash.plan.missing", path = plan.relative())
    }
}

/// The words of `args`, with `-o/--output FILE` (or `--name NAME`) taken
/// out.
fn split_flag<'a>(args: &'a str, flags: &[&str]) -> Result<(Vec<&'a str>, Option<&'a str>), Error> {
    let mut words = Vec::new();
    let mut value = None;
    let mut it = args.split_whitespace();
    while let Some(word) = it.next() {
        if flags.contains(&word) {
            let given = it
                .next()
                .ok_or_else(|| Error::new(&codes::CLI_USAGE, format!("{word} needs a value")))?;
            value = Some(given);
        } else {
            words.push(word);
        }
    }
    Ok((words, value))
}

/// `path` against the session's working directory.
fn in_cwd(ctx: &Ctx<'_>, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        ctx.live.place.cwd().join(path)
    }
}

fn home(ctx: &Ctx<'_>) -> Result<pmt_config::Home, Error> {
    pmt_config::Home::open(ctx.scope.home())
}

/// `/export [profile] [-o <file>]`: `prometheus profile export` with its
/// defaults (no sessions, no secrets); the session's profile and
/// `<profile>.tar.zst` in the working directory when omitted.
pub(super) fn export(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let (words, output) = split_flag(args, &["-o", "--output"])?;
    let profile = match words.as_slice() {
        [] => ctx.scope.name().to_string(),
        [one] => (*one).to_owned(),
        _ => {
            return Err(Error::new(
                &codes::CLI_USAGE,
                "usage: /export [profile] [-o <file>]",
            ));
        }
    };
    let output = in_cwd(ctx, output.unwrap_or(&format!("{profile}.tar.zst")));
    let manifest =
        crate::profile_cmd::archive::export_archive(&home(ctx)?, &profile, &output, false, false)?;
    Ok(pmt_i18n::t!(
        ctx.strings(),
        "slash.archive.exported",
        profile = manifest.profile,
        members = manifest.members.len(),
        path = output.display()
    ))
}

/// `/import <archive> [--name <name>]`: `prometheus profile import`; an
/// archive with sealed secrets needs the command's passphrase prompt.
pub(super) fn import(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let (words, name) = split_flag(args, &["--name", "--as"])?;
    let [file] = words.as_slice() else {
        return Err(Error::new(
            &codes::CLI_USAGE,
            "usage: /import <archive> [--name <name>]",
        ));
    };
    let file = in_cwd(ctx, file);
    let (manifest, profile) =
        crate::profile_cmd::archive::import_archive(&home(ctx)?, &file, name, false)?;
    Ok(pmt_i18n::t!(
        ctx.strings(),
        "slash.archive.imported",
        profile = manifest.profile,
        path = file.display(),
        name = profile
    ))
}
