//! `prometheus skills list|show|lint`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pmt_config::schema::SkillsConfig;
use pmt_core::error::{Error, codes};
use pmt_skills::catalog::{Missing, Provenance};
use pmt_skills::layout::{self, SKILL_FILE};
use pmt_skills::lint::{self, Finding, LintContext};
use pmt_skills::{Skill, eject, gates};
use serde::Serialize;

use super::Env;
use crate::output::{document, print_raw};

/// One row of `skills list`.
#[derive(Debug, Serialize)]
struct Row {
    name: String,
    category: String,
    source: String,
    status: String,
    description: String,
    path: String,
}

/// The status column: enabled, disabled (everywhere or on surfaces),
/// unsupported platform, shadowed.
fn status(config: &SkillsConfig, skill: &Skill) -> String {
    if gates::disabled(config, None, &skill.name) {
        return "disabled".to_owned();
    }
    if !gates::platform_ok(skill, gates::host_platform()) {
        return "unsupported platform".to_owned();
    }
    let surfaces: Vec<&str> = config
        .platform_disabled
        .iter()
        .filter(|(_, names)| names.contains(&skill.name))
        .map(|(platform, _)| platform.as_str())
        .collect();
    if surfaces.is_empty() {
        "enabled".to_owned()
    } else {
        format!("disabled on {}", surfaces.join(", "))
    }
}

fn row(skill: &Skill, status: String) -> Row {
    Row {
        name: skill.name.clone(),
        category: skill.category.clone(),
        source: skill.tier.as_str().to_owned(),
        status,
        description: skill.description.clone(),
        path: skill.dir.display(),
    }
}

/// `prometheus skills list [--outdated]`.
pub(super) fn list(env: &Env, outdated: bool) -> Result<u8, Error> {
    if outdated {
        return list_outdated(env);
    }
    let catalog = env.catalog();
    let config = &env.scope.config().skills;
    let mut rows: Vec<Row> = catalog
        .skills
        .iter()
        .map(|s| row(s, status(config, s)))
        .chain(
            catalog
                .shadowed
                .iter()
                .map(|s| row(s, "shadowed".to_owned())),
        )
        .collect();
    rows.sort_by(|a, b| (&a.category, &a.name, &a.source).cmp(&(&b.category, &b.name, &b.source)));
    if env.json {
        return document(
            "skills list",
            &serde_json::json!({"profile": env.scope.name().as_str(), "skills": rows, "notices": catalog.notices}),
        );
    }
    let table: Vec<[&str; 4]> = rows
        .iter()
        .map(|r| [&*r.name, &*r.category, &*r.source, &*r.status])
        .collect();
    let mut text = columns(&["NAME", "CATEGORY", "SOURCE", "STATUS"], &table);
    for notice in &catalog.notices {
        let _ = writeln!(text, "{notice}");
    }
    print_raw(&text)
}

fn list_outdated(env: &Env) -> Result<u8, Error> {
    let outdated = eject::outdated(env.scope.dir(), &env.bundled);
    if env.json {
        let rows: Vec<_> = outdated
            .iter()
            .map(
                |o| serde_json::json!({"name": o.name, "ejected": o.ejected, "bundled": o.bundled}),
            )
            .collect();
        return document(
            "skills list",
            &serde_json::json!({"profile": env.scope.name().as_str(), "outdated": rows}),
        );
    }
    let mut text = String::new();
    for o in &outdated {
        let why = if o.bundled.is_some() {
            "bundled version changed"
        } else {
            "no longer bundled"
        };
        let _ = writeln!(
            text,
            "{}  {why}; see prometheus skills diff {} or prometheus skills reset {}",
            o.name, o.name, o.name
        );
    }
    if text.is_empty() {
        text.push_str("no outdated skills\n");
    }
    print_raw(&text)
}

/// Left-aligned columns separated by two spaces.
fn columns<const N: usize>(header: &[&str; N], rows: &[[&str; N]]) -> String {
    let mut widths = header.map(str::len);
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let mut text = String::new();
    for row in std::iter::once(header).chain(rows) {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            if i + 1 == N {
                line.push_str(cell);
            } else {
                let _ = write!(line, "{cell:<width$}  ", width = widths[i]);
            }
        }
        let _ = writeln!(text, "{}", line.trim_end());
    }
    text
}

fn provenance(p: Provenance) -> &'static str {
    match p {
        Provenance::Builtin => "builtin",
        Provenance::Local => "local",
        Provenance::Hub(_) => "hub",
        Provenance::Unreadable => "unreadable",
    }
}

/// `prometheus skills show <name>`.
pub(super) fn show(env: &Env, name: &str) -> Result<u8, Error> {
    let catalog = env.catalog();
    let skill = catalog.lookup(name).map_err(|missing| match missing {
        Missing::Ambiguous(paths) => Error::new(
            &codes::SKILL_AMBIGUOUS,
            Missing::ambiguous_message(name, &paths),
        ),
        Missing::NotFound => Error::new(
            &codes::SKILL_NOT_FOUND,
            pmt_skills::hints::not_found(env.scope.home(), env.scope.name().as_str(), name),
        ),
    })?;
    let text = skill
        .dir
        .read(SKILL_FILE)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let status = status(&env.scope.config().skills, &skill);
    let files = skill.dir.files();
    if env.json {
        return document(
            "skills show",
            &serde_json::json!({
                "name": skill.name, "description": skill.description, "category": skill.category,
                "source": skill.tier.as_str(), "path": skill.dir.display(),
                "provenance": provenance(skill.provenance), "status": status,
                "files": files, "content": text,
            }),
        );
    }
    let mut out = String::new();
    let _ = writeln!(out, "name: {}", skill.name);
    let _ = writeln!(out, "description: {}", skill.description);
    let _ = writeln!(out, "category: {}", skill.category);
    let _ = writeln!(
        out,
        "source: {} ({})",
        skill.tier.as_str(),
        skill.dir.display()
    );
    let _ = writeln!(out, "provenance: {}", provenance(skill.provenance));
    let _ = writeln!(out, "status: {status}");
    let _ = writeln!(out, "files: {}", files.join(", "));
    let _ = write!(out, "\n{text}");
    print_raw(&out)
}

/// One linted skill.
#[derive(Debug, Serialize)]
struct Linted {
    path: String,
    findings: Vec<Finding>,
}

/// The skill directories below each path (a `SKILL.md`, a skill
/// directory, or a directory holding skills).
fn skill_dirs(paths: &[PathBuf]) -> Result<Vec<PathBuf>, Error> {
    let mut dirs = Vec::new();
    for path in paths {
        if path.is_file() {
            dirs.push(path.parent().unwrap_or(Path::new(".")).to_path_buf());
            continue;
        }
        if !path.is_dir() {
            return Err(Error::new(
                &codes::SKILL_IO,
                format!("{}: no such file or directory", path.display()),
            ));
        }
        let files = layout::walk(path);
        for rel in layout::skill_dirs(files.iter().map(String::as_str)) {
            dirs.push(if rel.is_empty() {
                path.clone()
            } else {
                path.join(rel)
            });
        }
    }
    Ok(dirs)
}

/// `prometheus skills lint [<path>...] [--strict]`; without paths, every
/// skill of the profile (bundled included).
pub(super) fn lint(env: &Env, paths: &[PathBuf], strict: bool) -> Result<u8, Error> {
    let catalog = env.catalog();
    let chars =
        usize::try_from(env.scope.config().skills.index_description_chars).unwrap_or(usize::MAX);
    let resolves = |n: &str| catalog.lookup(n).is_ok();
    let lint_one = |text: &str, dir_name: &str| {
        lint::lint(
            text,
            &LintContext {
                dir_name,
                description_chars: chars,
                resolves: &resolves,
            },
        )
    };
    let mut results = Vec::new();
    if paths.is_empty() {
        for skill in &catalog.skills {
            let text = skill.dir.read(SKILL_FILE).unwrap_or_default();
            results.push(Linted {
                path: format!("{}/{SKILL_FILE}", skill.dir.display()),
                findings: lint_one(&String::from_utf8_lossy(&text), &skill.dir_name),
            });
        }
    } else {
        for dir in skill_dirs(paths)? {
            let file = dir.join(SKILL_FILE);
            let text = std::fs::read_to_string(&file)
                .map_err(|e| Error::new(&codes::SKILL_IO, format!("{}: {e}", file.display())))?;
            let dir_name = dir
                .canonicalize()
                .unwrap_or_else(|_| dir.clone())
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            results.push(Linted {
                path: file.display().to_string(),
                findings: lint_one(&text, &dir_name),
            });
        }
    }
    let found = results.iter().any(|r| !r.findings.is_empty());
    let code = u8::from(strict && found);
    if env.json {
        document("skills lint", &serde_json::json!({"skills": results}))?;
        return Ok(code);
    }
    let mut text = String::new();
    for r in &results {
        for f in &r.findings {
            let _ = writeln!(text, "{}: {}: {}", r.path, f.rule, f.message);
        }
    }
    if text.is_empty() {
        let noun = if results.len() == 1 {
            "skill"
        } else {
            "skills"
        };
        let _ = writeln!(text, "{} {noun} checked, no findings", results.len());
    }
    print_raw(&text)?;
    Ok(code)
}
