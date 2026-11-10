//! A profile directory and a hub on the fake services.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_config::schema::{SkillSignaturePolicy, SkillsConfig};
use pmt_net::http::OutboundPolicy;
use pmt_skillhub::Hub;
use pmt_skillhub::http::Client;
use pmt_skillhub::testkit::FakeHub;
use pmt_skills::bundled::Bundled;
use tempfile::TempDir;

pub(crate) struct Env {
    pub(crate) fake: FakeHub,
    pub(crate) dir: TempDir,
    pub(crate) hub: Hub,
}

pub(crate) fn skill_md(name: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: Does {name} things.\n---\n# {name}\n\n{body}\n")
}

impl Env {
    pub(crate) fn new() -> Self {
        Self::with(SkillSignaturePolicy::Warn)
    }

    pub(crate) fn with(policy: SkillSignaturePolicy) -> Self {
        let fake = FakeHub::start();
        let dir = tempfile::Builder::new()
            .prefix("hub-")
            .tempdir_in(target_tmp())
            .unwrap();
        std::fs::create_dir_all(dir.path().join("skills")).unwrap();
        let config = SkillsConfig {
            signature_policy: policy,
            ..SkillsConfig::default()
        };
        let mut hub = Hub::new(
            Arc::new(Client::new(&OutboundPolicy::default()).unwrap()),
            fake.endpoints(),
            dir.path(),
            &config,
        );
        hub.official = Arc::new(Bundled::from_files([
            (
                "mlops/axolotl/SKILL.md",
                skill_md("axolotl", "Fine-tune with axolotl."),
            ),
            (
                "research/arxiv-tools/SKILL.md",
                skill_md("arxiv-tools", "Search arXiv."),
            ),
        ]));
        Self { fake, dir, hub }
    }

    pub(crate) fn profile(&self) -> &Path {
        self.dir.path()
    }

    pub(crate) fn skills(&self) -> PathBuf {
        self.dir.path().join("skills")
    }

    /// Re-reads the taps (after `taps::add`).
    pub(crate) fn reload_taps(&mut self) {
        self.hub.taps = pmt_skillhub::taps::read(self.dir.path()).unwrap();
    }
}

fn target_tmp() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-tmp");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Every file below `dir`, relative, with content.
pub(crate) fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = walk(dir, dir);
    out.sort();
    out
}

fn walk(root: &Path, dir: &Path) -> Vec<(String, Vec<u8>)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(root, &path));
        } else {
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            out.push((rel, std::fs::read(&path).unwrap()));
        }
    }
    out
}
