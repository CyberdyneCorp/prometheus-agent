//! `/skills` in a session and the config changes behind it (spec: skills,
//! Deferred versus immediate activation of skill changes; Disabled skills;
//! Skill mutation safety (pending writes); Project-local skills require
//! explicit trust).
//!
//! The typed command is the user's consent, so the change is written at
//! once; what is deferred is the *prompt*: the session keeps its frozen
//! skills index (and its prompt cache) and the change reaches the model as
//! a per-turn note, unless `--now` rebuilds the index, which the caller
//! applies with `Agent::rebuild_prompt(Rebuild::Now("skills"))` so the
//! trajectory records one `prompt.rebuilt` with reason `skills --now`.

use std::fmt::Write as _;
use std::path::Path;

use pmt_config::{Launch, Overrides, ProfileDir, ProfileScope};
use pmt_core::error::{Error, codes};

use crate::eject;
use crate::pending;
use crate::session::SkillsSession;
use crate::slash::SlashTable;

/// Printed for a deferred change.
pub const DEFERRED: &str = "Skill change takes effect in your next session. Use /new to start one now, or repeat with --now to apply immediately (invalidates prompt cache).";
/// Printed for an immediate change.
pub const APPLIED_NOW: &str =
    "Skill change applied to this session now (the prompt cache was invalidated).";
/// `/skills` usage.
pub const USAGE: &str = "usage: /skills [list] | enable|disable|reset <name> [--now] | install|uninstall <identifier> [--now] | pending | approve|reject <id>";

/// What the session does with a `/skills` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// Shown to the user.
    pub text: String,
    /// Rebuild the prompt now with `Rebuild::Now("skills")`.
    pub rebuild_now: bool,
}

fn usage() -> Error {
    Error::new(&codes::CLI_USAGE, USAGE)
}

/// Runs `/skills <args>`; `core` holds the core command names and aliases
/// (for the slash-command notes of the listing).
///
/// # Errors
///
/// `cli.usage`, config write errors (`config.invalid` for the essential
/// skill), `skill.*`.
pub fn run(session: &SkillsSession, args: &str, core: &[&str]) -> Result<Reply, Error> {
    let mut words: Vec<&str> = args.split_whitespace().collect();
    let now = words.contains(&"--now");
    words.retain(|w| *w != "--now");
    let text = match words.as_slice() {
        [] | ["list"] => return Ok(plain(listing(session, core))),
        ["pending"] => return Ok(plain(pending_listing(session))),
        ["approve", id] => {
            let outcome = pending::approve(session, id)?;
            return Ok(plain(outcome.message));
        }
        ["reject", id] => {
            pending::reject(&session.skills_dir(), id)?;
            return Ok(plain(format!("Rejected pending skill write {id}.")));
        }
        ["enable", name] => set_disabled(session.scope().profile(), name, false)?,
        ["disable", name] => set_disabled(session.scope().profile(), name, true)?,
        ["reset", name] => {
            let (catalog, _) = session.catalog();
            eject::reset(session.scope().dir(), &catalog, name)?;
            format!("Reset '{name}' to the bundled version.")
        }
        ["install" | "uninstall", _] => {
            return Err(Error::new(
                &codes::SKILL_HUB_UNAVAILABLE,
                "installing and removing hub skills ships with the skills hub (M2)",
            ));
        }
        _ => return Err(usage()),
    };
    if now {
        session.refresh_now();
        return Ok(Reply {
            text: format!("{text}\n{APPLIED_NOW}"),
            rebuild_now: true,
        });
    }
    Ok(plain(format!("{text}\n{DEFERRED}")))
}

fn plain(text: String) -> Reply {
    Reply {
        text,
        rebuild_now: false,
    }
}

/// Adds `name` to (or removes it from) `skills.disabled`.
///
/// # Errors
///
/// Config write errors; disabling the essential skill is `config.invalid`.
pub fn set_disabled(profile: &ProfileDir, name: &str, disabled: bool) -> Result<String, Error> {
    let file = profile.config_file();
    let mut names = current(profile)?.skills.disabled;
    names.retain(|n| n != name);
    if disabled {
        names.push(name.to_owned());
    }
    file.set("skills.disabled", &toml_list(&names))?;
    Ok(if disabled {
        format!("Disabled skill '{name}'.")
    } else {
        format!("Enabled skill '{name}'.")
    })
}

/// Adds the canonical `root` to `skills.trusted_project_dirs`
/// (`prometheus skills trust <root>`).
///
/// # Errors
///
/// `skill.io` when `root` does not exist; config write errors.
pub fn trust(profile: &ProfileDir, root: &Path) -> Result<String, Error> {
    let root = root
        .canonicalize()
        .map_err(|e| Error::new(&codes::SKILL_IO, format!("{}: {e}", root.display())))?;
    let file = profile.config_file();
    let mut dirs = current(profile)?.skills.trusted_project_dirs;
    let text = root.display().to_string();
    if !dirs.contains(&text) {
        dirs.push(text.clone());
    }
    file.set("skills.trusted_project_dirs", &toml_list(&dirs))?;
    Ok(format!(
        "Trusted {text}: its .prometheus/skills and .agents/skills load in new sessions."
    ))
}

/// The profile's config as on disk (no launch overrides).
fn current(profile: &ProfileDir) -> Result<pmt_config::Config, Error> {
    let overrides = Overrides::parse::<&str>(&[])?;
    Ok(
        ProfileScope::open(profile.clone(), &overrides, Launch::Other)?
            .config()
            .clone(),
    )
}

fn toml_list(items: &[String]) -> String {
    let quoted: Vec<String> = items
        .iter()
        .map(|i| toml::Value::String(i.clone()).to_string())
        .collect();
    format!("[{}]", quoted.join(", "))
}

fn listing(session: &SkillsSession, core: &[&str]) -> String {
    let (catalog, config) = session.catalog();
    let visible = session.visible(&catalog, &config);
    let table = SlashTable::new(visible.iter().copied(), core);
    let mut text = String::new();
    for skill in &visible {
        let _ = writeln!(
            text,
            "{} ({}, {}): {}",
            skill.name,
            skill.category,
            skill.tier.as_str(),
            skill.description
        );
        if let Some(note) = table.note(&skill.name) {
            let _ = writeln!(text, "  {note}");
        }
    }
    for skill in &catalog.shadowed {
        let _ = writeln!(text, "{} ({}): shadowed", skill.name, skill.tier.as_str());
    }
    for notice in &catalog.notices {
        let _ = writeln!(text, "{notice}");
    }
    if text.is_empty() {
        text.push_str("No skills.\n");
    }
    text
}

fn pending_listing(session: &SkillsSession) -> String {
    let all = pending::list(&session.skills_dir());
    if all.is_empty() {
        return "No pending skill writes.".to_owned();
    }
    let mut text = String::new();
    for p in all {
        let _ = writeln!(
            text,
            "{}  {} {}  ({})",
            p.id,
            p.request.action.as_str(),
            p.request.name,
            p.created_at
        );
    }
    text
}
