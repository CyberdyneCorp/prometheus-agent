//! PR completion contracts (spec: kanban, PR completion contracts). A
//! task's contract is `local-only` (the default), a repository
//! `OWNER/REPO`, or one exact pull request. Under a repository contract a
//! completion names `metadata.published_pr`, the first matching PR is
//! bound for good, and the task completes only when every required check
//! of the PR's head commit passed. The GitHub reads happen in the binary
//! ([`PrChecks`]); this module parses contracts and PR URLs and judges the
//! evidence the binary read, so the verdict rules live in one place.

use std::collections::BTreeMap;
use std::fmt;

use pmt_core::error::Error;
use serde::Serialize;

use crate::model::invalid;

/// A task's completion contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Contract {
    /// No remote evidence.
    LocalOnly,
    /// Any pull request of `owner/repo`.
    Repo {
        /// The owner.
        owner: String,
        /// The repository.
        repo: String,
    },
    /// Exactly this pull request.
    Pull(PrRef),
}

/// One GitHub pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrRef {
    /// The owner.
    pub owner: String,
    /// The repository.
    pub repo: String,
    /// The pull request number.
    pub number: u64,
}

impl PrRef {
    /// Parses `https://github.com/OWNER/REPO/pull/<n>` (a trailing `/`,
    /// query or fragment is ignored).
    #[must_use]
    pub fn parse(url: &str) -> Option<Self> {
        let rest = url.trim().strip_prefix("https://github.com/")?;
        let rest = rest.split(['?', '#']).next()?.trim_end_matches('/');
        let mut parts = rest.split('/');
        let (owner, repo, pull, number) =
            (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() || pull != "pull" || !segment(owner) || !segment(repo) {
            return None;
        }
        Some(Self {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            number: number.parse().ok().filter(|n| *n > 0)?,
        })
    }

    /// The canonical URL.
    #[must_use]
    pub fn url(&self) -> String {
        format!(
            "https://github.com/{}/{}/pull/{}",
            self.owner, self.repo, self.number
        )
    }

    /// `owner/repo`, compared case-insensitively as GitHub does.
    fn same_repo(&self, owner: &str, repo: &str) -> bool {
        self.owner.eq_ignore_ascii_case(owner) && self.repo.eq_ignore_ascii_case(repo)
    }
}

fn segment(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

impl Contract {
    /// Parses `local-only`, `OWNER/REPO` or an exact pull-request URL.
    ///
    /// # Errors
    ///
    /// `kanban.invalid_task`.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let text = text.trim();
        if text == "local-only" {
            return Ok(Self::LocalOnly);
        }
        if text.starts_with("https://") {
            return PrRef::parse(text).map(Self::Pull).ok_or_else(|| {
                invalid(format!(
                    "invalid completion contract '{text}' (expected a https://github.com/OWNER/REPO/pull/<n> URL)"
                ))
            });
        }
        match text.split_once('/') {
            Some((owner, repo)) if segment(owner) && segment(repo) => Ok(Self::Repo {
                owner: owner.to_owned(),
                repo: repo.to_owned(),
            }),
            _ => Err(invalid(format!(
                "invalid completion contract '{text}' (expected OWNER/REPO, a pull-request URL or local-only)"
            ))),
        }
    }

    /// The contract a task stores (`None` is `local-only`).
    ///
    /// # Errors
    ///
    /// `kanban.invalid_task` for a stored value that no longer parses.
    pub fn of(stored: Option<&str>) -> Result<Self, Error> {
        stored.map_or(Ok(Self::LocalOnly), Self::parse)
    }

    /// Whether `pr` satisfies the contract's repository (or is its PR).
    #[must_use]
    pub fn admits(&self, pr: &PrRef) -> bool {
        match self {
            Self::LocalOnly => true,
            Self::Repo { owner, repo } => pr.same_repo(owner, repo),
            Self::Pull(want) => want.same_repo(&pr.owner, &pr.repo) && want.number == pr.number,
        }
    }

    /// The stored spelling (`None` for `local-only`).
    #[must_use]
    pub fn stored(&self) -> Option<String> {
        match self {
            Self::LocalOnly => None,
            other => Some(other.to_string()),
        }
    }
}

impl fmt::Display for Contract {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LocalOnly => f.write_str("local-only"),
            Self::Repo { owner, repo } => write!(f, "{owner}/{repo}"),
            Self::Pull(pr) => f.write_str(&pr.url()),
        }
    }
}

/// Why a PR's evidence refuses completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    /// A required check is missing, pending or not green.
    Checks,
    /// No usable credential for the assignee profile.
    Auth,
    /// The GitHub API could not be read.
    Infra,
    /// The repository requires no checks (nothing proves the PR).
    Policy,
}

/// A check run on the PR's head commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRun {
    /// Its id.
    pub id: i64,
    /// Its name (the context it reports).
    pub name: String,
    /// `queued`, `in_progress`, `completed`...
    pub status: String,
    /// `success`, `failure`, `cancelled`, `timed_out`, `skipped`,
    /// `neutral`, `stale`, `action_required` (when completed).
    pub conclusion: Option<String>,
}

/// A legacy commit status on the PR's head commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitStatus {
    /// Its id.
    pub id: i64,
    /// Its context.
    pub context: String,
    /// `success`, `pending`, `failure` or `error`.
    pub state: String,
}

/// What the binary read about a pull request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Evidence {
    /// The PR's current head commit.
    pub head_sha: String,
    /// Required contexts from branch protection and rulesets.
    pub required: Vec<String>,
    /// Check runs of the head commit.
    pub runs: Vec<CheckRun>,
    /// Legacy statuses of the head commit (newest first per context).
    pub statuses: Vec<CommitStatus>,
}

/// The verdict on a pull request: accepted, or why not (recorded as the
/// `pr_acceptance` event).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Acceptance {
    /// The PR's URL.
    pub pr: String,
    /// The head commit judged.
    pub head_sha: Option<String>,
    /// The required contexts.
    pub required: Vec<String>,
    /// The check runs and statuses consulted.
    pub check_ids: Vec<i64>,
    /// Each required context that is not green, with its state.
    pub failing: BTreeMap<String, String>,
    /// `None` when every required context passed.
    pub classification: Option<Classification>,
    /// Why, and what to do next.
    pub reason: String,
    /// The next step.
    pub next_step: String,
}

impl Acceptance {
    /// A refusal before any evidence (auth, infra).
    #[must_use]
    pub fn refused(pr: &PrRef, classification: Classification, reason: &str, next: &str) -> Self {
        Self {
            pr: pr.url(),
            head_sha: None,
            required: Vec::new(),
            check_ids: Vec::new(),
            failing: BTreeMap::new(),
            classification: Some(classification),
            reason: reason.to_owned(),
            next_step: next.to_owned(),
        }
    }

    /// Whether the PR may complete the task.
    #[must_use]
    pub fn accepted(&self) -> bool {
        self.classification.is_none()
    }
}

/// The state of one required context: `success` passes, anything else
/// (missing, pending, failed, cancelled, timed out, stale, skipped,
/// neutral) does not.
fn context_state(context: &str, evidence: &Evidence, ids: &mut Vec<i64>) -> String {
    let run = evidence.runs.iter().find(|r| r.name == context);
    if let Some(run) = run {
        ids.push(run.id);
        return if run.status == "completed" {
            run.conclusion
                .clone()
                .unwrap_or_else(|| "unknown".to_owned())
        } else {
            "pending".to_owned()
        };
    }
    match evidence.statuses.iter().find(|s| s.context == context) {
        Some(status) => {
            ids.push(status.id);
            status.state.clone()
        }
        None => "missing".to_owned(),
    }
}

/// Judges `evidence` of `pr`.
#[must_use]
pub fn judge(pr: &PrRef, evidence: &Evidence) -> Acceptance {
    let mut required: Vec<String> = evidence.required.clone();
    required.sort();
    required.dedup();
    let mut ids = Vec::new();
    let mut failing = BTreeMap::new();
    for context in &required {
        let state = context_state(context, evidence, &mut ids);
        if state != "success" {
            failing.insert(context.clone(), state);
        }
    }
    let (classification, reason, next) = if required.is_empty() {
        (
            Some(Classification::Policy),
            "the repository requires no status checks, so nothing proves the pull request"
                .to_owned(),
            "require status checks on the base branch (branch protection or a ruleset)".to_owned(),
        )
    } else if failing.is_empty() {
        (
            None,
            "every required check passed".to_owned(),
            String::new(),
        )
    } else {
        let list: Vec<String> = failing.iter().map(|(c, s)| format!("{c} ({s})")).collect();
        (
            Some(Classification::Checks),
            format!("required checks are not green: {}", list.join(", ")),
            "fix the failing checks, push, and complete again once they pass".to_owned(),
        )
    };
    Acceptance {
        pr: pr.url(),
        head_sha: Some(evidence.head_sha.clone()),
        required,
        check_ids: ids,
        failing,
        classification,
        reason,
        next_step: next,
    }
}

/// Reads a PR's evidence with the assignee profile's own GitHub
/// credential (the binary implements it over the GitHub API).
pub trait PrChecks: Send + Sync {
    /// The verdict on `pr` for a task assigned to `profile`.
    fn evaluate(&self, profile: &str, pr: &PrRef) -> Acceptance;
}

/// Whether a comment links a GitHub pull request.
#[must_use]
pub fn links_pr(text: &str) -> bool {
    text.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | '<' | '>' | '"' | '\''))
        .any(|word| PrRef::parse(word.trim_end_matches(['.', ',', ';'])).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr() -> PrRef {
        PrRef::parse("https://github.com/acme/api/pull/41").unwrap()
    }

    #[test]
    fn contracts_parse_and_admit_their_repository() {
        assert_eq!(Contract::parse("local-only").unwrap(), Contract::LocalOnly);
        let repo = Contract::parse("acme/api").unwrap();
        assert!(repo.admits(&pr()));
        assert!(!repo.admits(&PrRef::parse("https://github.com/acme/web/pull/1").unwrap()));
        let exact = Contract::parse("https://github.com/acme/api/pull/42").unwrap();
        assert!(!exact.admits(&pr()));
        assert!(Contract::parse("not a repo").is_err());
        assert!(Contract::parse("https://gitlab.com/a/b/pull/1").is_err());
    }

    #[test]
    fn a_red_or_missing_required_check_is_classified_checks() {
        let evidence = Evidence {
            head_sha: "abc".to_owned(),
            required: vec!["ci/test".to_owned(), "lint".to_owned()],
            runs: vec![CheckRun {
                id: 7,
                name: "ci/test".to_owned(),
                status: "completed".to_owned(),
                conclusion: Some("failure".to_owned()),
            }],
            statuses: Vec::new(),
        };
        let verdict = judge(&pr(), &evidence);
        assert_eq!(verdict.classification, Some(Classification::Checks));
        assert_eq!(verdict.failing["ci/test"], "failure");
        assert_eq!(verdict.failing["lint"], "missing");
        assert_eq!(verdict.check_ids, vec![7]);
    }

    #[test]
    fn no_required_checks_is_a_policy_refusal_and_green_is_accepted() {
        let none = judge(&pr(), &Evidence::default());
        assert_eq!(none.classification, Some(Classification::Policy));
        let green = Evidence {
            head_sha: "abc".to_owned(),
            required: vec!["build".to_owned()],
            runs: Vec::new(),
            statuses: vec![CommitStatus {
                id: 3,
                context: "build".to_owned(),
                state: "success".to_owned(),
            }],
        };
        assert!(judge(&pr(), &green).accepted());
    }

    #[test]
    fn comments_with_pr_links_are_recognised() {
        assert!(links_pr("opened https://github.com/acme/api/pull/41."));
        assert!(!links_pr("see https://github.com/acme/api/issues/41"));
    }
}
