//! `prometheus skills bundles list|show|create|delete|reload|install`.

use std::fmt::Write as _;

use pmt_core::error::{Error, codes};
use pmt_skills::bundles::{self, BundleFile, Member};
use pmt_skills::reload::{self, Changes};
use serde_json::json;

use super::Env;
use super::args::BundlesCommand;
use super::maintain::{print_changes, scan_now};
use crate::output::{json, print_raw};

/// Runs one `skills bundles` subcommand.
pub(super) fn run(env: &Env, command: &BundlesCommand) -> Result<u8, Error> {
    let profile = env.scope.dir();
    match command {
        BundlesCommand::List => {
            let all = bundles::load(profile);
            if env.json {
                let rows: Vec<_> = all.bundles.values().map(row).collect();
                let problems: Vec<_> = all
                    .problems
                    .iter()
                    .map(|(path, why)| json!({"path": path, "reason": why}))
                    .collect();
                return json(&json!({"bundles": rows, "problems": problems}));
            }
            print_raw(&bundles::listing(&all))
        }
        BundlesCommand::Show { slug } => show(env, slug),
        BundlesCommand::Create {
            slug,
            skills,
            description,
            instruction,
            force,
        } => {
            let file = BundleFile {
                name: None,
                description: description.clone(),
                skills: skills.iter().cloned().map(Member::Name).collect(),
                instruction: instruction.clone(),
            };
            let path = bundles::create(profile, slug, &file, *force)?;
            if env.json {
                return json(&json!({"slug": slug, "path": path}));
            }
            eprintln!("created bundle /{slug} at {}", path.display());
            Ok(0)
        }
        BundlesCommand::Delete { slug } => {
            let path = bundles::delete(profile, slug)?;
            if env.json {
                return json(&json!({"slug": slug, "deleted": path}));
            }
            eprintln!("deleted bundle /{slug} ({})", path.display());
            Ok(0)
        }
        BundlesCommand::Reload => reload_bundles(env),
        BundlesCommand::Install { slug } => install(env, slug),
    }
}

fn row(bundle: &bundles::Bundle) -> serde_json::Value {
    json!({
        "slug": bundle.slug,
        "name": bundle.name,
        "description": bundle.file.description,
        "skills": bundle.file.skills,
        "instruction": bundle.file.instruction,
        "path": bundle.path,
    })
}

fn show(env: &Env, slug: &str) -> Result<u8, Error> {
    let bundle = bundles::find(env.scope.dir(), slug)?;
    if env.json {
        return json(&row(&bundle));
    }
    let catalog = env.catalog();
    let mut text = String::new();
    let _ = writeln!(text, "/{}  {}", bundle.slug, bundle.path.display());
    if let Some(description) = &bundle.file.description {
        let _ = writeln!(text, "description: {description}");
    }
    for member in &bundle.file.skills {
        let state = if catalog.lookup(member.name()).is_ok() {
            "installed"
        } else {
            "missing"
        };
        let _ = write!(text, "skill: {} ({state}", member.name());
        if let Some(source) = member.source() {
            let _ = write!(text, ", source {source}");
        }
        text.push_str(")\n");
    }
    if let Some(instruction) = &bundle.file.instruction {
        let _ = writeln!(text, "instruction: {instruction}");
    }
    print_raw(&text)
}

/// Compares the bundles with the last recorded scan and records them (the
/// skills part of the record is kept for `skills reload`).
fn reload_bundles(env: &Env) -> Result<u8, Error> {
    let file = reload::state_file(env.scope.dir());
    let before = reload::read_state(&file);
    let mut now = before.clone();
    now.bundles = scan_now(env).bundles;
    reload::write_state(&file, &now)?;
    print_changes(env, &Changes::between(&before, &now))
}

/// Installs the missing members that name a hub `source`: the hub
/// pipeline ships in M2, so this refuses when anything needs installing.
fn install(env: &Env, slug: &str) -> Result<u8, Error> {
    let bundle = bundles::find(env.scope.dir(), slug)?;
    let catalog = env.catalog();
    let missing: Vec<&Member> = bundle
        .file
        .skills
        .iter()
        .filter(|m| catalog.lookup(m.name()).is_err())
        .collect();
    let installable: Vec<String> = missing
        .iter()
        .filter_map(|m| m.source().map(|s| format!("{} ({s})", m.name())))
        .collect();
    if !installable.is_empty() {
        return Err(Error::new(
            &codes::SKILL_HUB_UNAVAILABLE,
            format!(
                "installing {} needs the skills hub, which ships in M2",
                installable.join(", ")
            ),
        ));
    }
    let unsourced: Vec<&str> = missing.iter().map(|m| m.name()).collect();
    let message = if unsourced.is_empty() {
        format!("every skill of /{slug} is installed")
    } else {
        format!(
            "nothing to install; missing skills without a source: {}",
            unsourced.join(", ")
        )
    };
    if env.json {
        return json(&json!({"slug": slug, "installed": [], "missing": unsourced}));
    }
    eprintln!("{message}");
    Ok(0)
}
