//! The `skills` component (spec: hermes-migration, Skills imported with
//! scanning).
//!
//! User-authored and hub-installed skills are copied into
//! `<profile>/skills/` with their category directories; Hermes bundled
//! skills whose content still matches `.bundled_manifest` stay behind
//! (Prometheus ships its own). Every copied skill runs the Prometheus
//! skill scan first: a `dangerous` or unscannable skill goes to
//! `skills/.quarantine/` and is disabled. Hub provenance goes to the
//! profile's `skills.lock`, and the Hermes disabled list to
//! `skills.disabled`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use md5::{Digest, Md5};
use pmt_core::error::{Error, codes};
use pmt_skills::layout;
use pmt_skills::lockfile::{self, LockFile, Trust};
use pmt_skills::scan::{PatternScanner, Scanner, Verdict};
use serde_json::Value;

use crate::component::Component;
use crate::config::json_to_toml;
use crate::plan::{Dest, Source, Step};
use crate::report::{Disposition, Entry};
use crate::writes::Writes;

/// Where quarantined skills go, below the profile's `skills/`.
pub const QUARANTINE_DIR: &str = ".quarantine";

/// Python runtime caches a dir hash and a copy leave out.
const CACHE_DIRS: &[&str] = &["__pycache__", ".pytest_cache", ".mypy_cache", ".ruff_cache"];

/// One skill to copy.
struct Copy {
    rel: String,
    source: PathBuf,
    quarantined: bool,
    hub: Option<lockfile::Entry>,
    entry: usize,
}

/// The planned skills component.
pub struct SkillsStep {
    entries: Vec<Entry>,
    copies: Vec<Copy>,
    disabled: Vec<String>,
    disabled_entry: Option<usize>,
    writes: Writes,
}

impl Step for SkillsStep {
    fn component(&self) -> Component {
        Component::Skills
    }

    fn entries(&mut self) -> &mut Vec<Entry> {
        &mut self.entries
    }

    fn apply(&mut self, dest: &Dest<'_>) -> Result<(), Error> {
        let skills = dest.scope.dir().join("skills");
        let lock = LockFile::in_dir(&skills);
        for c in &self.copies {
            if let Some(why) = copy_skill(c, &skills, &lock, dest.overwrite)? {
                let e = &mut self.entries[c.entry];
                e.disposition = Disposition::Conflict;
                e.reason = why;
            }
        }
        let current = &dest.scope.config().skills.disabled;
        let mut disabled: Vec<String> = current.clone();
        for name in &self.disabled {
            if !disabled.contains(name) {
                disabled.push(name.clone());
            }
        }
        if disabled != *current {
            let index = self.disabled_entry.unwrap_or(0);
            let list = toml_edit::Value::Array(disabled.iter().map(String::as_str).collect());
            self.writes.key("skills.disabled", list, index);
        }
        // The list is merged with the profile's, so it never conflicts.
        self.writes.apply(dest.scope, &mut self.entries, true)
    }
}

/// Copies one skill (and records its hub provenance); `Some(why)` when an
/// existing copy with other content was kept.
fn copy_skill(
    c: &Copy,
    skills: &Path,
    lock: &LockFile,
    overwrite: bool,
) -> Result<Option<String>, Error> {
    let target = if c.quarantined {
        skills.join(QUARANTINE_DIR).join(&c.rel)
    } else {
        skills.join(&c.rel)
    };
    if target.exists() {
        if pmt_skills::digest::dir(&target) == digest_of(&c.source) {
            return Ok(None);
        }
        if !overwrite {
            return Ok(Some(format!(
                "{} exists with other content; kept (use --overwrite to replace it)",
                c.rel
            )));
        }
        std::fs::remove_dir_all(&target)
            .map_err(|e| Error::io_doing(&codes::HOME_IO, "cannot replace", &target, &e))?;
    }
    copy_tree(&c.source, &target)?;
    if let Some(hub) = &c.hub {
        lock.upsert(lockfile::Entry {
            content_digest: pmt_skills::digest::dir(&target),
            ..hub.clone()
        })?;
    }
    Ok(None)
}

/// Plans the skills component.
///
/// # Errors
///
/// `migrate.component` when the skills directory cannot be read.
pub fn plan(src: &Source<'_>) -> Result<SkillsStep, Error> {
    let mut step = SkillsStep {
        entries: Vec::new(),
        copies: Vec::new(),
        disabled: Vec::new(),
        disabled_entry: None,
        writes: Writes::default(),
    };
    let root = src.profile.path("skills");
    if root.is_dir() {
        let bundled = manifest(&root.join(".bundled_manifest"));
        let hub = hub_lock(&root.join(".hub").join("lock.json"));
        let files: Vec<String> = layout::walk(&root)
            .into_iter()
            .filter(|f| !f.starts_with('.') && !f.split('/').any(|p| CACHE_DIRS.contains(&p)))
            .collect();
        for rel in layout::skill_dirs(files.iter().map(String::as_str)) {
            if rel.is_empty() {
                continue;
            }
            plan_skill(&root, &rel, &bundled, &hub, &mut step);
        }
    }
    plan_disabled(src, &mut step);
    Ok(step)
}

fn plan_skill(
    root: &Path,
    rel: &str,
    bundled: &BTreeMap<String, String>,
    hub: &BTreeMap<String, Value>,
    step: &mut SkillsStep,
) {
    let dir = root.join(rel);
    let name = rel.rsplit('/').next().unwrap_or(rel).to_owned();
    let item = format!("skills/{rel}");
    let entry = |d| Entry::new(Component::Skills, item.clone(), d);
    let mut e = entry(Disposition::Mapped).to(format!("skills/{rel}"));
    if let Some(origin) = bundled.get(&name) {
        if hermes_hash(&dir) == *origin {
            step.entries.push(
                entry(Disposition::Skipped)
                    .because("unchanged Hermes bundled skill; Prometheus ships its own"),
            );
            return;
        }
        e = entry(Disposition::Transformed)
            .to(format!("skills/{rel}"))
            .because("locally modified bundled skill");
    }
    let provenance = hub.get(&name).map(|h| hub_entry(&name, rel, h));
    if let Some(h) = &provenance {
        e.reason = join(&e.reason, &format!("hub skill from {}", h.source));
    }
    let report = scan(&dir);
    let quarantined = match &report {
        Ok(r) if r.verdict == Verdict::Dangerous => {
            e = entry(Disposition::Failed)
                .to(format!("skills/{QUARANTINE_DIR}/{rel}"))
                .because(format!(
                    "quarantined and disabled: scan found {}",
                    r.summary()
                ));
            true
        }
        Err(why) => {
            e = entry(Disposition::Failed)
                .to(format!("skills/{QUARANTINE_DIR}/{rel}"))
                .because(format!("quarantined and disabled: the scan failed ({why})"));
            true
        }
        Ok(r) => {
            if r.verdict == Verdict::Caution {
                e.reason = join(&e.reason, &format!("scan: {}", r.summary()));
            }
            false
        }
    };
    if !quarantined && has_python(&dir) {
        e.reason = join(&e.reason, "requires the Python bridge (Python scripts)");
    }
    if quarantined {
        step.disabled.push(name);
    }
    step.copies.push(Copy {
        rel: rel.to_owned(),
        source: dir,
        quarantined,
        hub: provenance.filter(|_| !quarantined),
        entry: step.entries.len(),
    });
    step.entries.push(e);
}

fn join(a: &str, b: &str) -> String {
    if a.is_empty() {
        b.to_owned()
    } else {
        format!("{a}; {b}")
    }
}

fn plan_disabled(src: &Source<'_>, step: &mut SkillsStep) {
    if let Some(list) = src.get("skills.disabled") {
        let names: Vec<String> = match list {
            Value::Array(items) => items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            Value::String(one) => vec![one.clone()],
            _ => Vec::new(),
        };
        step.disabled_entry = Some(step.entries.len());
        step.entries.push(
            Entry::new(Component::Skills, "skills.disabled", Disposition::Mapped)
                .to("skills.disabled"),
        );
        step.disabled.extend(names);
    } else if !step.disabled.is_empty() {
        step.disabled_entry = step.copies.iter().find(|c| c.quarantined).map(|c| c.entry);
    }
    if let Some(map) = src.get("skills.platform_disabled") {
        let index = step.entries.len();
        let entry = Entry::new(
            Component::Skills,
            "skills.platform_disabled",
            Disposition::Mapped,
        )
        .to("skills.platform_disabled");
        match json_to_toml(map) {
            Some(value) => {
                step.entries.push(entry);
                step.writes.key("skills.platform_disabled", value, index);
            }
            None => step.entries.push(Entry {
                disposition: Disposition::Unsupported,
                reason: "not a mapping of platform to skill names".to_owned(),
                ..entry
            }),
        }
    }
}

fn scan(dir: &Path) -> Result<pmt_skills::scan::Report, String> {
    let files: Vec<(String, Vec<u8>)> = layout::walk(dir)
        .into_iter()
        .map(|rel| std::fs::read(dir.join(&rel)).map(|bytes| (rel, bytes)))
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    PatternScanner.scan(&files)
}

fn has_python(dir: &Path) -> bool {
    layout::walk(dir)
        .iter()
        .any(|f| Path::new(f).extension().is_some_and(|e| e == "py"))
}

/// `.bundled_manifest`: `name:md5` per line (v1 lines carry no hash).
fn manifest(path: &Path) -> BTreeMap<String, String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter_map(|l| l.trim().split_once(':'))
        .map(|(n, h)| (n.trim().to_owned(), h.trim().to_owned()))
        .collect()
}

/// `.hub/lock.json`'s `installed` map.
fn hub_lock(path: &Path) -> BTreeMap<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("installed").and_then(Value::as_object).cloned())
        .map(|m| m.into_iter().collect())
        .unwrap_or_default()
}

fn hub_entry(name: &str, rel: &str, h: &Value) -> lockfile::Entry {
    let text = |k: &str| h.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
    let source = match (text("source"), text("identifier")) {
        (s, i) if !s.is_empty() && !i.is_empty() => format!("{s}:{i}"),
        (s, i) => format!("hermes-hub:{}", if i.is_empty() { s } else { i }),
    };
    let trust = match text("trust_level").as_str() {
        "builtin" | "official" => Trust::Builtin,
        "trusted" => Trust::Trusted,
        _ => Trust::Community,
    };
    lockfile::Entry {
        name: name.to_owned(),
        source,
        resolved_ref: None,
        content_digest: String::new(),
        trust: Some(trust),
        signature: Some("unsigned".to_owned()),
        installed_at: lockfile::now(),
        category: rel.rsplit_once('/').map(|(c, _)| c.to_owned()),
        path: Some(rel.to_owned()),
        quarantined: None,
    }
}

/// Hermes' `dir_hash`: MD5 over each file's relative path and bytes in
/// path order, runtime caches left out.
fn hermes_hash(dir: &Path) -> String {
    let mut files: Vec<String> = layout::walk(dir)
        .into_iter()
        .filter(|f| !f.split('/').any(|p| CACHE_DIRS.contains(&p)))
        .filter(|f| {
            let path = Path::new(f);
            !(path.extension().is_some_and(|e| e == "pyc" || e == "pyo")
                && dir.join(path.with_extension("py")).is_file())
        })
        .collect();
    files.sort();
    let mut hasher = Md5::new();
    for f in files {
        hasher.update(f.as_bytes());
        hasher.update(std::fs::read(dir.join(&f)).unwrap_or_default());
    }
    hasher.finalize().iter().fold(String::new(), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    })
}

fn digest_of(dir: &Path) -> String {
    pmt_skills::digest::dir(dir)
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), Error> {
    let io =
        |path: &Path, e: std::io::Error| Error::io_doing(&codes::HOME_IO, "cannot write", path, &e);
    for rel in layout::walk(from) {
        if rel.split('/').any(|p| CACHE_DIRS.contains(&p)) {
            continue;
        }
        let target = to.join(&rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
        }
        std::fs::copy(from.join(&rel), &target).map_err(|e| io(&target, e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Fixture;

    #[test]
    fn an_unchanged_bundled_skill_stays_behind() {
        let f = Fixture::new("", "");
        let skill = "---\nname: arxiv\ndescription: Papers.\n---\nSearch arXiv.\n";
        let dir = f.dir.path().join("skills/research/arxiv");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), skill).unwrap();
        // Hermes' dir_hash: MD5 over each relative path and its bytes.
        let mut md5 = Md5::new();
        md5.update(b"SKILL.md");
        md5.update(skill.as_bytes());
        let hash = md5.finalize().iter().fold(String::new(), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        });
        std::fs::write(
            f.dir.path().join("skills/.bundled_manifest"),
            format!("arxiv:{hash}\n"),
        )
        .unwrap();
        let step = plan(&f.source()).unwrap();
        assert_eq!(step.entries.len(), 1);
        assert_eq!(step.entries[0].disposition, Disposition::Skipped);
        assert!(step.copies.is_empty());
    }
}
