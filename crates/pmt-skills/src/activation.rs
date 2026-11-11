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
use pmt_core::text::Catalog;

use crate::eject;
use crate::pending;
use crate::session::SkillsSession;
use crate::slash::SlashTable;

/// The catalog key printed for a deferred change.
pub const DEFERRED: &str = "slash.skills.deferred";
/// The catalog key printed for an immediate change.
pub const APPLIED_NOW: &str = "slash.skills.applied_now";
/// `/skills` usage.
pub const USAGE: &str = "usage: /skills [list] | enable|disable|reset <name> [--now] | pending | diff <id> | approve|reject <id>|all | approval on|off | browse | search <query> | inspect|install <identifier> | uninstall|accept <name> | check | update | audit | tap ... | untrust <root>";

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

/// Runs `/skills <args>` in the profile's language; `core` holds the core
/// command names and aliases (for the slash-command notes of the listing).
///
/// # Errors
///
/// `cli.usage`, config write errors (`config.invalid` for the essential
/// skill), `skill.*`.
pub fn run(session: &SkillsSession, args: &str, core: &[&str]) -> Result<Reply, Error> {
    let strings = pmt_config::language::strings(session.scope().dir(), pmt_i18n::Surface::Core);
    run_in(&*strings, session, args, core)
}

/// [`run`] worded with `text` (`prometheus skills pending` passes English).
///
/// # Errors
///
/// As [`run`].
pub fn run_in(
    text: &dyn Catalog,
    session: &SkillsSession,
    args: &str,
    core: &[&str],
) -> Result<Reply, Error> {
    let catalog = text;
    let mut words: Vec<&str> = args.split_whitespace().collect();
    let now = words.contains(&"--now");
    words.retain(|w| *w != "--now");
    if let Some(text) = review_command(catalog, session, &words)? {
        return Ok(plain(text));
    }
    let profile = session.scope().profile();
    let text = match words.as_slice() {
        [] | ["list"] => return Ok(plain(listing(catalog, session, core))),
        ["enable", name] => set_disabled(catalog, profile, name, None, false)?,
        ["disable", name] => set_disabled(catalog, profile, name, None, true)?,
        ["reset", name] => {
            let (skills, _) = session.catalog();
            eject::reset(session.scope().dir(), &skills, name)?;
            catalog.text("slash.skills.reset", &[("name", name)])
        }
        _ => return Err(usage()),
    };
    if now {
        session.refresh_now();
        return Ok(Reply {
            text: format!("{text}\n{}", catalog.text(APPLIED_NOW, &[])),
            rebuild_now: true,
        });
    }
    Ok(plain(format!("{text}\n{}", catalog.text(DEFERRED, &[]))))
}

/// The write-approval commands (`pending`, `diff`, `approve`, `reject`,
/// `approval`); `None` for any other command.
fn review_command(
    catalog: &dyn Catalog,
    session: &SkillsSession,
    words: &[&str],
) -> Result<Option<String>, Error> {
    let profile = session.scope().dir();
    let none = || catalog.text("slash.skills.no_pending", &[]);
    let text = match words {
        ["pending"] => pending_listing(session).unwrap_or_else(none),
        ["diff", id] => pending::fit(
            &pending::diff(session, id)?,
            session.setup().message_limit,
            id,
        ),
        ["approve", target] => {
            let mut lines = Vec::new();
            for id in pending::ids(profile, target) {
                lines.push(pending::approve(session, &id)?.message);
            }
            done(&lines, none)
        }
        ["reject", target] => {
            let mut lines = Vec::new();
            for id in pending::ids(profile, target) {
                pending::reject(profile, &id)?;
                lines.push(catalog.text("slash.skills.rejected", &[("id", &id)]));
            }
            done(&lines, none)
        }
        ["approval", state @ ("on" | "off")] => {
            let on = *state == "on";
            session
                .scope()
                .profile()
                .config_file()
                .set("skills.write_approval", if on { "true" } else { "false" })?;
            let key = if on {
                "slash.skills.approval_on"
            } else {
                "slash.skills.approval_off"
            };
            catalog.text(key, &[])
        }
        _ => return Ok(None),
    };
    Ok(Some(text))
}

fn done(lines: &[String], empty: impl FnOnce() -> String) -> String {
    if lines.is_empty() {
        empty()
    } else {
        lines.join("\n")
    }
}

fn plain(text: String) -> Reply {
    Reply {
        text,
        rebuild_now: false,
    }
}

/// Adds `name` to (or removes it from) `skills.disabled`, or
/// `skills.platform_disabled.<platform>` when a platform is given; the
/// confirmation is worded with `text`.
///
/// # Errors
///
/// Config write errors; disabling the essential skill is `config.invalid`.
pub fn set_disabled(
    text: &dyn Catalog,
    profile: &ProfileDir,
    name: &str,
    platform: Option<&str>,
    disabled: bool,
) -> Result<String, Error> {
    let file = profile.config_file();
    let skills = current(profile)?.skills;
    let (key, mut names) = match platform {
        None => ("skills.disabled".to_owned(), skills.disabled),
        Some(p) => (
            format!("skills.platform_disabled.{p}"),
            skills.platform_disabled.get(p).cloned().unwrap_or_default(),
        ),
    };
    names.retain(|n| n != name);
    if disabled {
        names.push(name.to_owned());
    }
    file.set(&key, &toml_list(&names))?;
    Ok(match (platform, disabled) {
        (None, true) => text.text("slash.skills.disabled", &[("name", &name)]),
        (None, false) => text.text("slash.skills.enabled", &[("name", &name)]),
        (Some(p), true) => text.text(
            "slash.skills.disabled_on",
            &[("name", &name), ("platform", &p)],
        ),
        (Some(p), false) => text.text(
            "slash.skills.enabled_on",
            &[("name", &name), ("platform", &p)],
        ),
    })
}

/// Adds the canonical `root` to `skills.trusted_project_dirs`
/// (`prometheus skills trust <root>`).
///
/// # Errors
///
/// `skill.io` when `root` does not exist; config write errors.
pub fn trust(text: &dyn Catalog, profile: &ProfileDir, root: &Path) -> Result<String, Error> {
    let root = root
        .canonicalize()
        .map_err(|e| Error::new(&codes::SKILL_IO, format!("{}: {e}", root.display())))?;
    let file = profile.config_file();
    let mut dirs = current(profile)?.skills.trusted_project_dirs;
    let shown = root.display().to_string();
    if !dirs.contains(&shown) {
        dirs.push(shown.clone());
    }
    file.set("skills.trusted_project_dirs", &toml_list(&dirs))?;
    Ok(text.text("slash.skills.trusted", &[("root", &shown)]))
}

/// Removes `root` (as given, or canonical) from
/// `skills.trusted_project_dirs` (`prometheus skills untrust <root>`).
///
/// # Errors
///
/// `skill.not_found` when `root` is not trusted; config write errors.
pub fn untrust(text: &dyn Catalog, profile: &ProfileDir, root: &Path) -> Result<String, Error> {
    let given = root.display().to_string();
    let canonical = root.canonicalize().ok().map(|p| p.display().to_string());
    let mut dirs = current(profile)?.skills.trusted_project_dirs;
    let before = dirs.len();
    dirs.retain(|d| *d != given && Some(d) != canonical.as_ref());
    if dirs.len() == before {
        return Err(Error::new(
            &codes::SKILL_NOT_FOUND,
            format!("{given} is not a trusted project root"),
        ));
    }
    profile
        .config_file()
        .set("skills.trusted_project_dirs", &toml_list(&dirs))?;
    Ok(text.text("slash.skills.untrusted", &[("root", &given)]))
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

fn listing(catalog_text: &dyn Catalog, session: &SkillsSession, core: &[&str]) -> String {
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
        let shadowed = catalog_text.text("slash.skills.shadowed", &[]);
        let _ = writeln!(text, "{} ({}): {shadowed}", skill.name, skill.tier.as_str());
    }
    for notice in &catalog.notices {
        let _ = writeln!(text, "{notice}");
    }
    if text.is_empty() {
        let _ = writeln!(text, "{}", catalog_text.text("slash.skills.none", &[]));
    }
    text
}

/// The pending writes; `None` when there are none.
fn pending_listing(session: &SkillsSession) -> Option<String> {
    let all = pending::list(session.scope().dir());
    if all.is_empty() {
        return None;
    }
    let mut text = String::new();
    for p in all {
        let _ = writeln!(text, "{}", p.gist());
    }
    Some(text)
}
