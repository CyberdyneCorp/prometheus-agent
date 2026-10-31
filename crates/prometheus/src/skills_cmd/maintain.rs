//! `prometheus skills reload|pending|opt-out|opt-in|list-modified|repair-official`.

use std::fmt::Write as _;
use std::sync::Arc;

use pmt_core::error::Error;
use pmt_skills::bundled::Bundled;
use pmt_skills::official::{self, Options};
use pmt_skills::reload::{self, Changes};
use pmt_skills::{SessionSetup, SkillsSession, activation, gates, optout};
use pmt_store::Store;
use serde_json::json;

use super::Env;
use super::args::{PendingCommand, SkillsCommand};
use crate::output::{json, print_raw};
use crate::prompt::{self, NEEDS_CONFIRMATION};

/// The maintenance subcommands.
pub(super) fn run(env: &Env, command: &SkillsCommand) -> Result<u8, Error> {
    match command {
        SkillsCommand::Reload => reload_all(env),
        SkillsCommand::Pending { command } => pending(env, command.as_ref()),
        SkillsCommand::OptOut { remove, yes } => opt_out(env, *remove, *yes),
        SkillsCommand::OptIn => {
            optout::set_included(env.scope.profile(), true)?;
            done(env, "bundled skills are shown again from the next session")
        }
        SkillsCommand::ListModified => list_modified(env),
        SkillsCommand::RepairOfficial {
            target,
            restore,
            yes,
        } => repair_official(env, target, *restore, *yes),
        _ => unreachable!("handled by skills_cmd::run"),
    }
}

fn done(env: &Env, message: &str) -> Result<u8, Error> {
    if env.json {
        return json(&json!({"profile": env.scope.name().as_str(), "message": message}));
    }
    eprintln!("{message}");
    Ok(0)
}

/// The scan the CLI compares a reload against (no session: the surface is
/// `cli`).
pub(super) fn scan_now(env: &Env) -> reload::Scan {
    let catalog = env.catalog();
    let config = &env.scope.config().skills;
    let view = gates::SessionView {
        platform: Some("cli".to_owned()),
        ..gates::SessionView::default()
    };
    let visible: Vec<&pmt_skills::Skill> = catalog
        .skills
        .iter()
        .filter(|s| gates::hidden(config, &view, s).is_none())
        .map(AsRef::as_ref)
        .collect();
    reload::scan_of(&visible, env.scope.dir())
}

fn reload_all(env: &Env) -> Result<u8, Error> {
    let file = reload::state_file(env.scope.dir());
    let before = reload::read_state(&file);
    let now = scan_now(env);
    reload::write_state(&file, &now)?;
    print_changes(env, &Changes::between(&before, &now))
}

pub(super) fn print_changes(env: &Env, changes: &Changes) -> Result<u8, Error> {
    if env.json {
        return json(changes);
    }
    print_raw(&format!("{}\n", changes.render()))
}

/// A session-less view of the profile's skills for the pending commands.
fn session(env: &Env) -> Result<SkillsSession, Error> {
    let store = Arc::new(Store::open(&env.scope)?);
    let mut setup = SessionSetup::new(env.scope.clone(), store, "cli");
    setup.user_home.clone_from(&env.user_home);
    setup.bundled = Arc::clone(&env.bundled);
    setup.view.platform = Some("cli".to_owned());
    Ok(SkillsSession::new(setup))
}

fn pending(env: &Env, command: Option<&PendingCommand>) -> Result<u8, Error> {
    let args = match command {
        None | Some(PendingCommand::List) => "pending".to_owned(),
        Some(PendingCommand::Diff { id }) => format!("diff {id}"),
        Some(PendingCommand::Approve { target }) => format!("approve {target}"),
        Some(PendingCommand::Reject { target }) => format!("reject {target}"),
    };
    let reply = activation::run(&session(env)?, &args, &[])?;
    if env.json {
        return json(&json!({"profile": env.scope.name().as_str(), "message": reply.text}));
    }
    let mut text = reply.text;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    print_raw(&text)
}

fn opt_out(env: &Env, remove: bool, yes: bool) -> Result<u8, Error> {
    let mut removed = Vec::new();
    if remove {
        let catalog = env.profile_catalog();
        let copies: Vec<_> = optout::ejected(env.scope.dir(), &catalog)
            .into_iter()
            .filter(optout::Copy::identical)
            .collect();
        if !copies.is_empty() && !yes {
            let names: Vec<&str> = copies.iter().map(|c| c.name.as_str()).collect();
            let what = format!("delete the unedited ejected skills {}", names.join(", "));
            match prompt::confirm_typed(&what, "delete")? {
                None => {
                    eprintln!("refusing to {what} without a terminal; pass --yes");
                    return Ok(NEEDS_CONFIRMATION);
                }
                Some(false) => return Ok(1),
                Some(true) => {}
            }
        }
        removed = optout::remove_identical(env.scope.dir(), &catalog)?;
    }
    optout::set_included(env.scope.profile(), false)?;
    if env.json {
        return json(
            &json!({"profile": env.scope.name().as_str(), "include_bundled": false, "removed": removed}),
        );
    }
    for name in &removed {
        eprintln!("removed unedited ejected skill '{name}'");
    }
    eprintln!("bundled skills are hidden from the next session (prometheus-agent stays)");
    Ok(0)
}

fn list_modified(env: &Env) -> Result<u8, Error> {
    let modified = optout::modified(env.scope.dir(), &env.profile_catalog());
    if env.json {
        let rows: Vec<_> = modified
            .iter()
            .map(|c| json!({"name": c.name, "path": c.path, "origin": c.origin, "current": c.current}))
            .collect();
        return json(&json!({"profile": env.scope.name().as_str(), "modified": rows}));
    }
    let mut text = String::new();
    for c in &modified {
        let _ = writeln!(text, "{}  {}", c.name, c.path.display());
    }
    if text.is_empty() {
        text.push_str("no modified ejected skills\n");
    }
    print_raw(&text)
}

fn repair_official(env: &Env, target: &str, restore: bool, yes: bool) -> Result<u8, Error> {
    if restore && !yes {
        let what = format!(
            "replace missing or modified official skills ({target}); current copies move to skills/.backups/"
        );
        match prompt::confirm_typed(&what, "restore")? {
            None => {
                eprintln!("refusing to restore without a terminal; pass --yes");
                return Ok(NEEDS_CONFIRMATION);
            }
            Some(false) => return Ok(1),
            Some(true) => {}
        }
    }
    repair_with(env, &Bundled::official(), target, restore)
}

/// Runs the repair against `official` and prints the result.
fn repair_with(env: &Env, official: &Bundled, target: &str, restore: bool) -> Result<u8, Error> {
    let repairs = official::repair(
        env.scope.dir(),
        &env.profile_catalog(),
        official,
        Options { target, restore },
    )?;
    if env.json {
        return json(&json!({"profile": env.scope.name().as_str(), "repairs": repairs}));
    }
    let mut text = String::new();
    for r in &repairs {
        let what = match r.action {
            official::Action::Recorded => "provenance recorded".to_owned(),
            official::Action::Restored => match &r.backup {
                Some(backup) => format!("restored (previous copy at {})", backup.display()),
                None => "restored".to_owned(),
            },
            official::Action::Missing => "missing; --restore installs it".to_owned(),
            official::Action::Modified => "modified; --restore replaces it".to_owned(),
        };
        let _ = writeln!(text, "{} ({}): {what}", r.name, r.source);
    }
    if text.is_empty() {
        text.push_str("no official skills installed\n");
    }
    print_raw(&text)
}
