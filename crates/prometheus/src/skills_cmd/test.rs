//! `prometheus skills test <name>|--all` (spec: skills, Skill evals): runs
//! each case of the skills' `evals/evals.toml` in an isolated temporary
//! profile, reports per-case pass/fail and the pass rate, writes a JSON
//! report under `<profile>/evals/skills/`, and exits 0 only when every
//! case passed.

use std::fmt::Write as _;
use std::sync::Arc;

use pmt_core::error::{Error, codes};
use pmt_skills::catalog::{Missing, files_of};
use pmt_skills::evals::EVALS_FILE;
use pmt_store::Store;
use serde_json::json;

use super::Env;
use super::evals::{Provider, Runner, SkillReport};
use crate::output::{json, print_raw};

/// What `skills test` was asked.
pub(crate) struct Request<'a> {
    pub(crate) name: Option<&'a str>,
    pub(crate) provider: Provider,
    pub(crate) repeat: u32,
}

pub(super) fn run(env: &Env, request: &Request<'_>) -> Result<u8, Error> {
    let catalog = env.catalog();
    let skills = match request.name {
        Some(name) => {
            let skill = catalog.lookup(name).map_err(|missing| match missing {
                Missing::Ambiguous(paths) => Error::new(
                    &codes::SKILL_AMBIGUOUS,
                    Missing::ambiguous_message(name, &paths),
                ),
                Missing::NotFound => {
                    Error::new(&codes::SKILL_NOT_FOUND, format!("no skill '{name}'"))
                }
            })?;
            vec![skill]
        }
        None => catalog.skills.clone(),
    };
    let store = Arc::new(Store::open(&env.scope)?);
    let runner = Runner::new(
        &env.scope,
        &store,
        &env.launch,
        request.provider,
        request.repeat,
    );
    let mut reports = Vec::new();
    for skill in &skills {
        let files = files_of(skill);
        if !files.iter().any(|(rel, _)| rel == EVALS_FILE) {
            continue;
        }
        if let Some(report) = runner.skill(&skill.name, &files)? {
            reports.push(report);
        }
    }
    if reports.is_empty() {
        if let Some(name) = request.name {
            return Err(Error::new(
                &codes::SKILL_INVALID,
                format!("skill '{name}' has no {EVALS_FILE}"),
            ));
        }
        eprintln!("no skill has {EVALS_FILE}");
        return Ok(0);
    }
    let path = save(env, request, &reports)?;
    let passed = reports.iter().all(SkillReport::passed);
    if env.json {
        json(&json!({"skills": reports, "passed": passed, "report": path}))?;
    } else {
        print_raw(&render(&reports))?;
        eprintln!("report: {}", path.display());
    }
    Ok(u8::from(!passed))
}

fn save(
    env: &Env,
    request: &Request<'_>,
    reports: &[SkillReport],
) -> Result<std::path::PathBuf, Error> {
    let dir = env.scope.path("evals/skills");
    let io = |e: std::io::Error| Error::new(&codes::SKILL_IO, format!("{}: {e}", dir.display()));
    std::fs::create_dir_all(&dir).map_err(io)?;
    let id = format!(
        "skills_{}",
        ulid::Ulid::generate().to_string().to_lowercase()
    );
    let path = dir.join(format!("{id}.json"));
    let body = json!({
        "id": id,
        "at": jiff::Timestamp::now().to_string(),
        "provider": request.provider,
        "repeat": request.repeat,
        "skills": reports,
    });
    let text = serde_json::to_string_pretty(&body).unwrap_or_default();
    std::fs::write(&path, text).map_err(io)?;
    Ok(path)
}

fn render(reports: &[SkillReport]) -> String {
    let mut text = String::new();
    for report in reports {
        let _ = writeln!(text, "{}", report.skill);
        for case in &report.cases {
            let mark = if case.passed { "pass" } else { "FAIL" };
            let _ = writeln!(
                text,
                "  {mark} {} ({}/{} runs)",
                case.name, case.passed_runs, case.runs
            );
            for failure in &case.failures {
                let _ = writeln!(text, "       {failure}");
            }
        }
        let passed = report.cases.iter().filter(|c| c.passed).count();
        let _ = writeln!(
            text,
            "{}: {passed} of {} cases passed; pass rate {} ({:.0}%)",
            report.skill,
            report.cases.len(),
            report.rate,
            report.rate.fraction() * 100.0
        );
    }
    text
}
