//! `prometheus skills publish` (spec: skills, Skill publishing and
//! snapshots): lint and scan the skill, sign it when a key is configured,
//! then open a pull request (GitHub: fork, new branch, one commit per
//! file, pull request against the default branch, never a push to it) or
//! a ClawHub registry submission, and return the resulting URL.

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use pmt_skills::layout::{self, SKILL_FILE};
use pmt_skills::lint::{self, LintContext};
use pmt_skills::scan::Verdict;
use serde_json::{Value, json};

use crate::http::{Request, Response};
use crate::hub::Hub;
use crate::sign::{self, SIGNATURE_FILE};

/// Where to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A pull request to `owner/repo`.
    GitHub {
        /// The repository the pull request targets.
        repo: String,
    },
    /// The ClawHub registry.
    ClawHub,
}

/// A finished publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    /// The skill.
    pub name: String,
    /// The pull request or submission URL.
    pub url: String,
    /// A `SKILL.sig` was written.
    pub signed: bool,
}

fn refused(why: impl Into<String>) -> Error {
    Error::new(&codes::SKILL_PUBLISH_REFUSED, why)
}

fn read_files(dir: &Path) -> Result<BTreeMap<String, Vec<u8>>, Error> {
    layout::walk(dir)
        .into_iter()
        .map(|rel| {
            let path = dir.join(&rel);
            std::fs::read(&path)
                .map(|b| (rel, b))
                .map_err(|e| Error::io(&codes::SKILL_IO, &path, &e))
        })
        .collect()
}

/// Lints, scans and (with `key`) signs the skill at `dir`, then
/// publishes it to `target`.
///
/// # Errors
///
/// `skill.publish_refused` (lint findings, a `dangerous` verdict),
/// `skill.signature`, `skill.hub_unavailable` (no token, a refused API
/// call), `skill.io`.
pub fn publish(
    hub: &Hub,
    dir: &Path,
    target: &Target,
    key: Option<&Path>,
    lint_ctx: &LintContext<'_>,
) -> Result<Published, Error> {
    let text = std::fs::read_to_string(dir.join(SKILL_FILE))
        .map_err(|e| Error::io(&codes::SKILL_IO, &dir.join(SKILL_FILE), &e))?;
    let findings = lint::lint(&text, lint_ctx);
    if !findings.is_empty() {
        let list: Vec<String> = findings
            .iter()
            .map(|f| format!("{}: {}", f.rule, f.message))
            .collect();
        return Err(refused(format!("lint findings: {}", list.join("; "))));
    }
    let name = lint_ctx.dir_name.to_owned();
    let mut files = read_files(dir)?;
    let scan: Vec<(String, Vec<u8>)> = files.iter().map(|(p, b)| (p.clone(), b.clone())).collect();
    let report = hub
        .scanner
        .scan(&scan)
        .map_err(|e| refused(format!("security scan failed: {e}")))?;
    if report.verdict == Verdict::Dangerous {
        return Err(refused(format!("security scan: {}", report.summary())));
    }
    let signed = match key {
        Some(key) => {
            files.remove(SIGNATURE_FILE);
            let digest =
                pmt_skills::digest::tree(files.iter().map(|(p, b)| (p.clone(), b.as_slice())));
            let text = sign::render(&sign::sign_minisign(&digest, key)?);
            let path = dir.join(SIGNATURE_FILE);
            private_fs::write_atomic(&path, text.as_bytes())
                .map_err(|e| Error::io(&codes::SKILL_IO, &path, &e))?;
            files.insert(SIGNATURE_FILE.to_owned(), text.into_bytes());
            true
        }
        None => false,
    };
    let url = match target {
        Target::GitHub { repo } => github(hub, repo, &name, &files)?,
        Target::ClawHub => clawhub(hub, &name, &files)?,
    };
    Ok(Published { name, url, signed })
}

fn send(hub: &Hub, request: Request) -> Result<(Response, Value), Error> {
    let url = request.url.clone();
    let response = hub
        .http
        .send(request)
        .map_err(|e| crate::http::unavailable(&url, &e))?;
    if !response.ok() {
        return Err(crate::http::unavailable(
            &url,
            &format!(
                "answered {}: {}",
                response.status,
                String::from_utf8_lossy(&response.body)
                    .chars()
                    .take(200)
                    .collect::<String>()
            ),
        ));
    }
    let value = response.json().unwrap_or_default();
    Ok((response, value))
}

fn token_missing(name: &str) -> Error {
    Error::new(
        &codes::SKILL_HUB_UNAVAILABLE,
        format!("publishing needs the {name} secret"),
    )
    .with_hint(format!("store it with `prometheus secrets set {name}`"))
}

/// Fork, branch, upload, pull request; the pull request URL.
fn github(
    hub: &Hub,
    repo: &str,
    name: &str,
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<String, Error> {
    if hub.github_token.is_none() {
        return Err(token_missing("GITHUB_TOKEN"));
    }
    let api = &hub.endpoints.github_api;
    let request = |url: String| hub.github(url);
    let (_, fork) = send(
        hub,
        request(format!("{api}/repos/{repo}/forks")).json("POST", &json!({})),
    )?;
    let fork_repo = fork["full_name"]
        .as_str()
        .ok_or_else(|| crate::http::unavailable(repo, "the fork answer names no repository"))?
        .to_owned();
    let (_, upstream) = send(hub, request(format!("{api}/repos/{repo}")))?;
    let base = upstream["default_branch"]
        .as_str()
        .unwrap_or("main")
        .to_owned();
    let (_, head) = send(
        hub,
        request(format!("{api}/repos/{fork_repo}/git/ref/heads/{base}")),
    )?;
    let sha = head["object"]["sha"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let stamp = jiff::Timestamp::now().strftime("%Y%m%d%H%M%S").to_string();
    let branch = format!("add-skill-{name}-{stamp}");
    send(
        hub,
        request(format!("{api}/repos/{fork_repo}/git/refs")).json(
            "POST",
            &json!({"ref": format!("refs/heads/{branch}"), "sha": sha}),
        ),
    )?;
    for (rel, bytes) in files {
        send(
            hub,
            request(format!(
                "{api}/repos/{fork_repo}/contents/skills/{name}/{rel}"
            ))
            .json(
                "PUT",
                &json!({
                    "message": format!("Add {name} skill: {rel}"),
                    "content": STANDARD.encode(bytes),
                    "branch": branch,
                }),
            ),
        )?;
    }
    let owner = fork_repo.split('/').next().unwrap_or_default();
    let (_, pull) = send(
        hub,
        request(format!("{api}/repos/{repo}/pulls")).json(
            "POST",
            &json!({
                "title": format!("Add skill: {name}"),
                "body": format!("Submitting the `{name}` skill. It passed `prometheus skills lint --strict` and the security scan before submission."),
                "head": format!("{owner}:{branch}"),
                "base": base,
            }),
        ),
    )?;
    pull["html_url"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| crate::http::unavailable(repo, "the pull request answer has no URL"))
}

/// A ClawHub submission; the submission URL.
fn clawhub(hub: &Hub, name: &str, files: &BTreeMap<String, Vec<u8>>) -> Result<String, Error> {
    let token = hub
        .clawhub_token
        .as_ref()
        .ok_or_else(|| token_missing("CLAWHUB_TOKEN"))?;
    let encoded: serde_json::Map<String, Value> = files
        .iter()
        .map(|(p, b)| (p.clone(), Value::String(STANDARD.encode(b))))
        .collect();
    let (_, answer) = send(
        hub,
        Request::get(format!("{}/skills", hub.endpoints.clawhub))
            .header("authorization", format!("Bearer {token}"))
            .json("POST", &json!({"slug": name, "files": encoded})),
    )?;
    answer["url"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| crate::http::unavailable("clawhub", "the submission answer has no URL"))
}
