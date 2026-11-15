//! The GitHub reads of PR completion contracts (spec: kanban, PR
//! completion contracts). For a task assigned to profile `p` the checker
//! uses `p`'s own `GITHUB_TOKEN` from its secrets store (never an ambient
//! login or the caller's credential) to read the pull request's head
//! commit, the base branch's required contexts (branch protection and
//! rulesets), and the head commit's check runs and legacy statuses at
//! `kanban.github_api_url`. It never writes to GitHub. The verdict itself
//! is `pmt_kanban::contract::judge`.

use std::path::PathBuf;

use pmt_config::{Launch, Overrides, ProfileName, ProfileScope};
use pmt_kanban::contract::{
    Acceptance, CheckRun, Classification, CommitStatus, Evidence, PrChecks, PrRef, judge,
};
use reqwest::StatusCode;
use serde_json::Value;

/// The secret a profile's GitHub credential is stored under.
pub const TOKEN_SECRET: &str = "GITHUB_TOKEN";

/// Reads GitHub evidence for the tasks of one home.
pub struct GitHub {
    home: PathBuf,
}

impl GitHub {
    /// The checker of the home at `home`.
    #[must_use]
    pub fn new(home: &std::path::Path) -> Self {
        Self {
            home: home.to_path_buf(),
        }
    }
}

/// A read that failed: the classification and why.
struct Failure(Classification, String);

impl PrChecks for GitHub {
    fn evaluate(&self, profile: &str, pr: &PrRef) -> Acceptance {
        let scope = match open_profile(&self.home, profile) {
            Ok(scope) => scope,
            Err(reason) => {
                return Acceptance::refused(
                    pr,
                    Classification::Auth,
                    &reason,
                    "check the assignee profile",
                );
            }
        };
        let Ok(token) = scope.secrets().resolve(TOKEN_SECRET) else {
            return Acceptance::refused(
                pr,
                Classification::Auth,
                &format!("not logged in for profile {profile}"),
                &format!("store a GitHub token as {TOKEN_SECRET} in profile {profile}'s secrets"),
            );
        };
        // The board calls this from synchronous code that may itself run
        // inside a runtime: the reads get a thread and runtime of their own.
        let read = std::thread::scope(|s| {
            s.spawn(|| crate::model_cmd::block_on(evidence(&scope, token.expose(), pr)))
                .join()
        });
        match read {
            Ok(Ok(Ok(evidence))) => judge(pr, &evidence),
            Ok(Ok(Err(Failure(class, reason)))) => {
                let next = match class {
                    Classification::Auth => "renew the profile's GitHub token",
                    _ => "retry once GitHub answers",
                };
                Acceptance::refused(pr, class, &reason, next)
            }
            _ => Acceptance::refused(
                pr,
                Classification::Infra,
                "the GitHub read could not run",
                "retry",
            ),
        }
    }
}

fn open_profile(home: &std::path::Path, profile: &str) -> Result<ProfileScope, String> {
    let home = pmt_config::layout::Home::open(home).map_err(|e| e.message().to_owned())?;
    let name = ProfileName::parse(profile).map_err(|e| e.message().to_owned())?;
    let dir = home
        .existing_profile(&name)
        .map_err(|e| e.message().to_owned())?;
    ProfileScope::open(dir, &Overrides::default(), Launch::Other)
        .map_err(|e| e.message().to_owned())
}

/// One authenticated GET; `Ok(None)` for a 404.
async fn get(client: &reqwest::Client, url: &str, token: &str) -> Result<Option<Value>, Failure> {
    let response = client
        .get(url)
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "prometheus-kanban")
        .send()
        .await
        .map_err(|e| Failure(Classification::Infra, format!("GitHub unreachable: {e}")))?;
    let status = response.status();
    if status == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err(Failure(
            Classification::Auth,
            format!(
                "GitHub refused the profile's credential (HTTP {})",
                status.as_u16()
            ),
        ));
    }
    if !status.is_success() {
        return Err(Failure(
            Classification::Infra,
            format!("GitHub answered HTTP {} for {url}", status.as_u16()),
        ));
    }
    response
        .json()
        .await
        .map(Some)
        .map_err(|e| Failure(Classification::Infra, format!("GitHub sent no JSON: {e}")))
}

fn strings(values: Option<&Value>, key: &str) -> Vec<String> {
    values
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.get(key).and_then(Value::as_str).or_else(|| v.as_str()))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The required contexts of `base`: branch protection's, then every
/// active ruleset's `required_status_checks` rule.
async fn required(
    client: &reqwest::Client,
    repo: &str,
    base: &str,
    token: &str,
) -> Result<Vec<String>, Failure> {
    let mut out = Vec::new();
    let protection = format!("{repo}/branches/{base}/protection/required_status_checks");
    if let Some(p) = get(client, &protection, token).await? {
        out.extend(strings(p.get("contexts"), "context"));
        out.extend(strings(p.get("checks"), "context"));
    }
    if let Some(Value::Array(rules)) =
        get(client, &format!("{repo}/rules/branches/{base}"), token).await?
    {
        for rule in rules
            .iter()
            .filter(|r| r["type"] == "required_status_checks")
        {
            out.extend(strings(
                rule.pointer("/parameters/required_status_checks"),
                "context",
            ));
        }
    }
    Ok(out)
}

async fn evidence(scope: &ProfileScope, token: &str, pr: &PrRef) -> Result<Evidence, Failure> {
    let config = scope.config();
    let client = crate::model_cmd::client(config)
        .map_err(|e| Failure(Classification::Infra, e.message().to_owned()))?;
    let api = config.kanban.github_api_url.trim_end_matches('/');
    let repo = format!("{api}/repos/{}/{}", pr.owner, pr.repo);
    let pull = get(&client, &format!("{repo}/pulls/{}", pr.number), token)
        .await?
        .ok_or_else(|| Failure(Classification::Infra, format!("{} not found", pr.url())))?;
    let head = pull["head"]["sha"].as_str().unwrap_or_default().to_owned();
    let base = pull["base"]["ref"].as_str().unwrap_or("main").to_owned();
    let required = required(&client, &repo, &base, token).await?;
    let runs = get(
        &client,
        &format!("{repo}/commits/{head}/check-runs?per_page=100"),
        token,
    )
    .await?
    .unwrap_or_default();
    let statuses = get(&client, &format!("{repo}/commits/{head}/status"), token)
        .await?
        .unwrap_or_default();
    Ok(Evidence {
        head_sha: head,
        required,
        runs: runs["check_runs"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| CheckRun {
                id: r["id"].as_i64().unwrap_or_default(),
                name: r["name"].as_str().unwrap_or_default().to_owned(),
                status: r["status"].as_str().unwrap_or_default().to_owned(),
                conclusion: r["conclusion"].as_str().map(str::to_owned),
            })
            .collect(),
        statuses: statuses["statuses"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|s| CommitStatus {
                id: s["id"].as_i64().unwrap_or_default(),
                context: s["context"].as_str().unwrap_or_default().to_owned(),
                state: s["state"].as_str().unwrap_or_default().to_owned(),
            })
            .collect(),
    })
}
