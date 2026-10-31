//! `prometheus skills` (spec: skills, Skill authoring lint; Bundled skills
//! are read-only and ejectable; Project-local skills require explicit
//! trust; Disabled skills; Skill bundles; /reload-skills rescans roots;
//! Skill write-approval review surface; Bundled skill opt-out and official
//! catalog repair; spec: cli).

pub mod args;
mod bundles;
mod list;
mod maintain;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_core::error::Error;
use pmt_skills::bundled::Bundled;
use pmt_skills::{Catalog, activation, eject};
use serde_json::json;

use crate::context::Context;
use crate::output::{json, print_raw};
use args::SkillsCommand;

/// What every `skills` subcommand works on.
pub(crate) struct Env {
    pub(crate) scope: ProfileScope,
    pub(crate) user_home: Option<PathBuf>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) bundled: Arc<Bundled>,
    pub(crate) json: bool,
}

impl Env {
    /// The profile's skills seen from the working directory.
    pub(crate) fn catalog(&self) -> Catalog {
        eject::profile_catalog(
            &self.scope,
            self.cwd.as_deref(),
            self.user_home.as_deref(),
            Arc::clone(&self.bundled),
        )
    }

    /// The profile's skills without the project tier.
    pub(crate) fn profile_catalog(&self) -> Catalog {
        eject::profile_catalog(
            &self.scope,
            None,
            self.user_home.as_deref(),
            Arc::clone(&self.bundled),
        )
    }
}

/// Runs one `skills` subcommand and returns its exit code.
///
/// # Errors
///
/// The command's failure; the caller prints it and exits with its code.
pub fn run(command: &SkillsCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let env = Env {
        scope: ctx.scope()?,
        user_home: ctx.launch.user_home().cloned(),
        cwd: std::env::current_dir().ok(),
        bundled: Bundled::builtin(),
        json: ctx.json,
    };
    match command {
        SkillsCommand::List { outdated } => list::list(&env, *outdated),
        SkillsCommand::Show { name } => list::show(&env, name),
        SkillsCommand::Lint { paths, strict } => list::lint(&env, paths, *strict),
        SkillsCommand::Enable { name, platform } => enable(&env, name, platform.as_deref(), false),
        SkillsCommand::Disable { name, platform } => enable(&env, name, platform.as_deref(), true),
        SkillsCommand::Trust { root } => trust(&env, root),
        SkillsCommand::Eject { name } => {
            let ejected = eject::eject(env.scope.dir(), &env.profile_catalog(), name)?;
            if env.json {
                return json(&json!({"name": ejected.name, "path": ejected.path}));
            }
            eprintln!("ejected '{}' to {}", ejected.name, ejected.path.display());
            Ok(0)
        }
        SkillsCommand::Diff { name } => print_raw(&eject::diff(
            env.scope.dir(),
            &env.profile_catalog(),
            &env.bundled,
            name,
        )?),
        SkillsCommand::Doctor => doctor(&env),
        SkillsCommand::Reset { name } => {
            eject::reset(env.scope.dir(), &env.profile_catalog(), name)?;
            if env.json {
                return json(&json!({"name": name, "reset": true}));
            }
            eprintln!("'{name}' now resolves to the bundled skill");
            Ok(0)
        }
        SkillsCommand::Bundles { command } => bundles::run(&env, command),
        other => maintain::run(&env, other),
    }
}

fn trust(env: &Env, root: &Path) -> Result<u8, Error> {
    let message = activation::trust(env.scope.profile(), root)?;
    if env.json {
        return json(
            &json!({"profile": env.scope.name().as_str(), "trusted": root.canonicalize().ok()}),
        );
    }
    eprintln!("{message}");
    Ok(0)
}

fn enable(env: &Env, name: &str, platform: Option<&str>, disabled: bool) -> Result<u8, Error> {
    let message = activation::set_disabled(env.scope.profile(), name, platform, disabled)?;
    if env.json {
        return json(&json!({"name": name, "platform": platform, "enabled": !disabled}));
    }
    eprintln!("{message} It applies from the next session.");
    Ok(0)
}

/// Prints every `SKILL.md` that did not load (`<path>:<line>: <reason>`)
/// and every quarantined project skill; exit 1 when there is any.
fn doctor(env: &Env) -> Result<u8, Error> {
    let catalog = env.catalog();
    let problems: Vec<_> = catalog
        .problems
        .iter()
        .map(|p| json!({"path": p.path, "line": p.line, "reason": p.reason}))
        .chain(catalog.quarantined.iter().map(|(skill, why)| {
            json!({"path": skill.dir.display(), "line": 0, "reason": format!("quarantined: {why}")})
        }))
        .collect();
    let code = u8::from(!problems.is_empty());
    if env.json {
        json(
            &json!({"profile": env.scope.name().as_str(), "problems": problems, "notices": catalog.notices}),
        )?;
        return Ok(code);
    }
    let mut text = String::new();
    for p in &problems {
        let line = p["line"].as_u64().unwrap_or(0);
        let path = p["path"].as_str().unwrap_or_default();
        let reason = p["reason"].as_str().unwrap_or_default();
        if line > 0 {
            let _ = writeln!(text, "{path}:{line}: {reason}");
        } else {
            let _ = writeln!(text, "{path}: {reason}");
        }
    }
    for notice in &catalog.notices {
        let _ = writeln!(text, "{notice}");
    }
    if text.is_empty() {
        text.push_str("no skill problems\n");
    }
    print_raw(&text)?;
    Ok(code)
}
