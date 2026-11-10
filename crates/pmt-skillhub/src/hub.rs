//! What every hub operation works with: HTTP, the endpoints, the profile's
//! trust settings and taps, and the embedded official catalog.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_config::schema::{SkillSignaturePolicy, SkillsConfig};
use pmt_skills::bundled::Bundled;
use pmt_skills::lockfile::Trust;
use pmt_skills::roots::profile_skills;
use pmt_skills::scan::{PatternScanner, Scanner};

use crate::http::{Endpoints, Http, Request};
use crate::keyless::TrustRoots;
use crate::taps::Tap;

/// The hub of one profile.
#[derive(Clone)]
pub struct Hub {
    /// HTTP access.
    pub http: Arc<dyn Http>,
    /// Where the services live.
    pub endpoints: Endpoints,
    /// The `GITHUB_TOKEN` secret, when set.
    pub github_token: Option<String>,
    /// The `CLAWHUB_TOKEN` secret, when set (publishing).
    pub clawhub_token: Option<String>,
    /// `skills.trusted_sources`.
    pub trusted: Vec<String>,
    /// `skills.signature_policy`.
    pub signature_policy: SkillSignaturePolicy,
    /// The profile's taps.
    pub taps: Vec<Tap>,
    /// The embedded official catalog.
    pub official: Arc<Bundled>,
    /// The profile directory.
    pub profile_dir: PathBuf,
    /// The security scan.
    pub scanner: Arc<dyn Scanner>,
    /// Keyless signature trust roots.
    pub roots: TrustRoots,
}

impl Hub {
    /// A hub over `http` for the profile at `profile_dir`.
    #[must_use]
    pub fn new(
        http: Arc<dyn Http>,
        endpoints: Endpoints,
        profile_dir: &Path,
        config: &SkillsConfig,
    ) -> Self {
        Self {
            http,
            endpoints,
            github_token: None,
            clawhub_token: None,
            trusted: config.trusted_sources.clone(),
            signature_policy: config.signature_policy,
            taps: crate::taps::read(profile_dir).unwrap_or_default(),
            official: Bundled::official(),
            profile_dir: profile_dir.to_path_buf(),
            scanner: Arc::new(PatternScanner),
            roots: TrustRoots::public(),
        }
    }

    /// The profile's `skills/` directory.
    #[must_use]
    pub fn skills_dir(&self) -> PathBuf {
        profile_skills(&self.profile_dir)
    }

    /// The hub's cache directory.
    #[must_use]
    pub fn cache_dir(&self) -> PathBuf {
        self.profile_dir.join("cache").join("skills-hub")
    }

    /// The trust of a GitHub repository `owner/repo`.
    #[must_use]
    pub fn repo_trust(&self, repo: &str) -> Trust {
        let trusted = self
            .trusted
            .iter()
            .any(|t| t.trim_end_matches('/').eq_ignore_ascii_case(repo));
        if trusted {
            Trust::Trusted
        } else {
            Trust::Community
        }
    }

    /// A GitHub API request, authenticated when the token is set.
    #[must_use]
    pub fn github(&self, url: String) -> Request {
        let request = Request::get(url).header("accept", "application/vnd.github+json");
        match &self.github_token {
            Some(token) => request.header("authorization", format!("Bearer {token}")),
            None => request,
        }
    }
}
