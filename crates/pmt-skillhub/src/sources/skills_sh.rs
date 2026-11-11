//! skills.sh: a directory of skills that live in GitHub repositories.
//! `skills-sh/<owner>/<repo>/<skill>` resolves to the repository's skill
//! directory whose name matches the slug or one of its aliases (the
//! directory name with or without the repository prefix, or the
//! frontmatter name).

use pmt_core::error::{Error, codes};
use pmt_skills::layout::{self, SKILL_FILE};

use crate::bundle::Bundle;
use crate::http::{Request, get_json};
use crate::hub::Hub;
use crate::ident::Identifier;
use crate::sources::{Listing, github};

/// Most `SKILL.md` files read to match an alias by frontmatter name.
const MAX_NAME_PROBES: usize = 40;

fn variants(repo: &str, skill: &str) -> Vec<String> {
    let mut out = vec![skill.to_owned()];
    if let Some(stripped) = skill.strip_prefix(&format!("{repo}-")) {
        out.push(stripped.to_owned());
    }
    out.push(format!("{repo}-{skill}"));
    out
}

/// The skill directory of `skill` in the repository files `paths`.
fn locate(hub: &Hub, repo_full: &str, sha: &str, skill: &str, paths: &[String]) -> Option<String> {
    let repo_name = repo_full.rsplit('/').next().unwrap_or(repo_full);
    let dirs = layout::skill_dirs(paths.iter().map(String::as_str));
    let names = variants(repo_name, skill);
    let by_dir = dirs.iter().find(|d| {
        let base = d.rsplit('/').next().unwrap_or(d);
        names.iter().any(|n| n == base)
    });
    if let Some(dir) = by_dir {
        return Some(dir.clone());
    }
    dirs.iter().take(MAX_NAME_PROBES).find_map(|dir| {
        let path = if dir.is_empty() {
            SKILL_FILE.to_owned()
        } else {
            format!("{dir}/{SKILL_FILE}")
        };
        let files = github::download(hub, repo_full, sha, dir, std::slice::from_ref(&path)).ok()?;
        let text = String::from_utf8_lossy(files.get(SKILL_FILE)?).into_owned();
        let front = pmt_skills::frontmatter::mapping(&text)?;
        let name = front.get("name")?.as_str()?;
        names.iter().any(|n| n == name).then(|| dir.clone())
    })
}

/// Fetches `skills-sh/<owner>/<repo>/<skill>`.
///
/// # Errors
///
/// `skill.not_found`, `skill.hub_unavailable`.
pub fn fetch(
    hub: &Hub,
    owner: &str,
    repo: &str,
    skill: &str,
    at: Option<&str>,
) -> Result<Bundle, Error> {
    let full = format!("{owner}/{repo}");
    let sha = github::commit(hub, &full, at)?;
    let paths = github::tree(hub, &full, &sha)?;
    let dir = locate(hub, &full, &sha, skill, &paths).ok_or_else(|| {
        Error::new(
            &codes::SKILL_NOT_FOUND,
            format!("skills.sh: no skill '{skill}' in {full}"),
        )
    })?;
    let id = Identifier::SkillsSh {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        skill: skill.to_owned(),
    };
    let mut bundle = github::fetch_dir(hub, id, &full, &sha, &dir, &paths)?;
    if let Some(found) = search(hub, skill, 20)
        .unwrap_or_default()
        .into_iter()
        .find(|l| l.identifier == format!("skills-sh/{full}/{skill}"))
    {
        bundle.upstream.installs = found.installs;
    }
    Ok(bundle)
}

/// Searches the directory.
///
/// # Errors
///
/// `skill.hub_unavailable`.
pub fn search(hub: &Hub, query: &str, limit: usize) -> Result<Vec<Listing>, Error> {
    let url = format!(
        "{}/api/search?q={}&limit={limit}",
        hub.endpoints.skills_sh,
        crate::sources::encode(query)
    );
    let value = get_json(hub.http.as_ref(), Request::get(url))?.unwrap_or_default();
    Ok(value["skills"]
        .as_array()
        .map(|items| items.iter().filter_map(|i| listing(hub, i)).collect())
        .unwrap_or_default())
}

fn listing(hub: &Hub, item: &serde_json::Value) -> Option<Listing> {
    let id = item["id"].as_str().map(str::to_owned).or_else(|| {
        Some(format!(
            "{}/{}",
            item["source"].as_str()?,
            item["skillId"].as_str()?
        ))
    })?;
    let mut parts = id.splitn(3, '/');
    let (owner, repo, skill) = (parts.next()?, parts.next()?, parts.next()?);
    let installs = item["installs"].as_u64();
    let name = item["name"].as_str().unwrap_or(skill).to_owned();
    Some(Listing {
        identifier: format!("skills-sh/{owner}/{repo}/{skill}"),
        name,
        description: format!("Indexed by skills.sh from {owner}/{repo}"),
        source: "skills-sh".to_owned(),
        trust: hub.repo_trust(&format!("{owner}/{repo}")),
        installs,
    })
}
