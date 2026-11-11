//! The steps other-agent importers are made of. Each holds its report
//! entries and writes through the same ledger rules as the Hermes
//! components ([`crate::ledger`]): unchanged sources are left alone and
//! destinations edited in Prometheus are kept as conflicts.

use std::path::{Path, PathBuf};

use pmt_config::grants::GrantsFile;
use pmt_core::error::{Error, codes};
use pmt_memory::manage::{self, Item, Limits};

use crate::component::Component;
use crate::ledger::{self, Decision};
use crate::memory::Fresh;
use crate::plan::{Dest, Existing, Step};
use crate::report::{Disposition, Entry};
use crate::skills::{self, QUARANTINE_DIR};
use crate::writes::{self, Writes};

/// Entries that write nothing (unsupported, failed or skipped items).
pub struct Notes {
    /// The component they belong to.
    pub component: Component,
    /// The entries.
    pub entries: Vec<Entry>,
}

impl Step for Notes {
    fn component(&self) -> Component {
        self.component
    }

    fn entries(&mut self) -> &mut Vec<Entry> {
        &mut self.entries
    }

    fn apply(&mut self, _: &Dest<'_>) -> Result<(), Error> {
        Ok(())
    }
}

/// Config keys and secrets.
pub struct WritesStep {
    /// The component.
    pub component: Component,
    /// The entries.
    pub entries: Vec<Entry>,
    /// What to write.
    pub writes: Writes,
}

impl Step for WritesStep {
    fn component(&self) -> Component {
        self.component
    }

    fn entries(&mut self) -> &mut Vec<Entry> {
        &mut self.entries
    }

    fn predict(&mut self, existing: &Existing<'_>) {
        self.writes.predict(existing, &mut self.entries);
    }

    fn apply(&mut self, dest: &Dest<'_>) -> Result<(), Error> {
        self.writes.apply(dest, &mut self.entries)
    }
}

/// Memory entries from one or more files, deduplicated against the
/// profile's memory and recorded with provenance `import:<source>`.
pub struct MemoryStep {
    /// One entry per source file.
    pub entries: Vec<Entry>,
    /// Entry index and ledger key of each file.
    pub files: Vec<(usize, String)>,
    /// The items, with the index of their file's entry.
    pub items: Vec<(usize, Item)>,
    /// `import:<source>`.
    pub provenance: String,
}

impl Step for MemoryStep {
    fn component(&self) -> Component {
        Component::Memory
    }

    fn entries(&mut self) -> &mut Vec<Entry> {
        &mut self.entries
    }

    fn apply(&mut self, dest: &Dest<'_>) -> Result<(), Error> {
        let fresh = Fresh::of(dest, &self.files, &self.items, &mut self.entries);
        let files: Vec<usize> = self.files.iter().map(|(i, _)| *i).collect();
        fresh.import(dest, |items| {
            let redactor = dest.scope.secrets().redactor()?;
            let summary = manage::import_from(
                dest.store,
                &dest.scope.config().memory,
                items,
                &redactor,
                Limits::Exceed,
                Some(&self.provenance),
            )?;
            if summary.duplicates > 0 {
                for i in files {
                    let e = &mut self.entries[i];
                    if e.disposition != Disposition::Skipped {
                        e.reason = format!(
                            "{}; {} already in memory, not added again",
                            e.reason, summary.duplicates
                        );
                    }
                }
            }
            Ok(())
        })
    }
}

/// One skill directory to copy.
pub struct SkillCopy {
    /// Its entry.
    pub entry: usize,
    /// The source directory.
    pub source: PathBuf,
    /// Where it goes below `<profile>/skills/`.
    pub rel: String,
    /// The scan failed: it goes to `.quarantine/` and is disabled.
    pub quarantined: bool,
}

/// What a skill does when the profile has another one at its target
/// (OpenClaw `--skill-conflict`; the other importers always `skip`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SkillConflict {
    /// Keep the profile's skill; report a conflict.
    #[default]
    Skip,
    /// Replace it.
    Overwrite,
    /// Import beside it as `<name>-imported`.
    Rename,
}

/// Skill directories copied under the same scan and quarantine as Hermes
/// skills.
pub struct SkillsStep {
    /// The entries.
    pub entries: Vec<Entry>,
    /// The copies.
    pub copies: Vec<SkillCopy>,
    /// What a conflicting skill does.
    pub on_conflict: SkillConflict,
}

impl SkillsStep {
    /// Plans copying `source` to `skills/<rel>` (scanned now).
    pub fn add(&mut self, item: String, source: PathBuf, rel: String) {
        let entry = Entry::new(Component::Skills, item, Disposition::Mapped);
        let (entry, quarantined) = match skills::screen(&source) {
            Err(why) => (
                Entry {
                    disposition: Disposition::Failed,
                    ..entry
                }
                .to(format!("skills/{QUARANTINE_DIR}/{rel}"))
                .because(why),
                true,
            ),
            Ok(note) => (
                entry
                    .to(format!("skills/{rel}"))
                    .because(note.unwrap_or_default()),
                false,
            ),
        };
        self.copies.push(SkillCopy {
            entry: self.entries.len(),
            source,
            rel,
            quarantined,
        });
        self.entries.push(entry);
    }
}

/// Installs one copy, applying the conflict policy.
fn install(
    on_conflict: SkillConflict,
    entries: &mut [Entry],
    c: &SkillCopy,
    target: &Path,
    dest: &Dest<'_>,
) -> Result<Decision, Error> {
    let key = format!("skill:{}", c.rel);
    if on_conflict == SkillConflict::Overwrite {
        let forced = Dest {
            overwrite: true,
            ..dest.clone()
        };
        return skills::install(&c.source, target, &key, &forced);
    }
    let decision = skills::install(&c.source, target, &key, dest)?;
    if decision != Decision::Conflict || on_conflict != SkillConflict::Rename {
        return Ok(decision);
    }
    let renamed = format!("{}-imported", c.rel);
    let to = target.with_file_name(format!(
        "{}-imported",
        target.file_name().unwrap_or_default().to_string_lossy()
    ));
    let decision = skills::install(&c.source, &to, &format!("skill:{renamed}"), dest)?;
    let e = &mut entries[c.entry];
    e.target = Some(format!("skills/{renamed}"));
    e.reason = format!("skills/{} exists; imported as skills/{renamed}", c.rel);
    Ok(decision)
}

impl Step for SkillsStep {
    fn component(&self) -> Component {
        Component::Skills
    }

    fn entries(&mut self) -> &mut Vec<Entry> {
        &mut self.entries
    }

    fn apply(&mut self, dest: &Dest<'_>) -> Result<(), Error> {
        let root = dest.scope.dir().join("skills");
        let mut disabled = dest.scope.config().skills.disabled.clone();
        for c in &self.copies {
            let target = if c.quarantined {
                root.join(QUARANTINE_DIR).join(&c.rel)
            } else {
                root.join(&c.rel)
            };
            let decision = install(self.on_conflict, &mut self.entries, c, &target, dest)?;
            let why = format!("skills/{} exists with other content", c.rel);
            writes::settle(decision, &mut self.entries[c.entry], &why);
            let name = c.rel.rsplit('/').next().unwrap_or(&c.rel).to_owned();
            if c.quarantined && !disabled.contains(&name) {
                disabled.push(name);
            }
        }
        if disabled != dest.scope.config().skills.disabled {
            let mut w = Writes::default();
            let list = toml_edit::Value::Array(disabled.iter().map(String::as_str).collect());
            w.key("skills.disabled", list, 0);
            w.apply_merged(dest, &mut self.entries)?;
        }
        Ok(())
    }
}

/// Command patterns: permanent approvals in `grants.toml` and user deny
/// rules in `approvals.deny`, recorded in the ledger as
/// `approval:<pattern>` and `deny:<pattern>`.
pub struct ApprovalsStep {
    /// The entries.
    pub entries: Vec<Entry>,
    /// Entry index and pattern of each approval.
    pub allow: Vec<(usize, String)>,
    /// Entry index and pattern of each deny rule.
    pub deny: Vec<(usize, String)>,
}

impl Step for ApprovalsStep {
    fn component(&self) -> Component {
        Component::Config
    }

    fn entries(&mut self) -> &mut Vec<Entry> {
        &mut self.entries
    }

    fn apply(&mut self, dest: &Dest<'_>) -> Result<(), Error> {
        let grants = GrantsFile::of(dest.scope.profile());
        let allowed = grants.load()?.allow;
        for (index, pattern) in &self.allow {
            let e = &mut self.entries[*index];
            let key = format!("approval:{pattern}");
            if settle_pattern(dest, e, &key, allowed.contains(pattern), "already approved") {
                dest.keep(grants.path())?;
                grants.allow_command(pattern)?;
            }
        }
        let current = &dest.scope.config().approvals.deny;
        let mut deny = current.clone();
        for (index, pattern) in &self.deny {
            let e = &mut self.entries[*index];
            let key = format!("deny:{pattern}");
            if settle_pattern(dest, e, &key, deny.contains(pattern), "already denied") {
                deny.push(pattern.clone());
            }
        }
        if deny != *current {
            let mut w = Writes::default();
            let list = toml_edit::Value::Array(deny.iter().map(String::as_str).collect());
            w.key("approvals.deny", list, self.deny[0].0);
            w.apply_merged(dest, &mut self.entries)?;
        }
        Ok(())
    }
}

/// Settles one command pattern against the ledger and records it; `true`
/// when it must be added. A pattern an earlier run imported is left
/// alone, so one the user removed in Prometheus stays removed.
fn settle_pattern(
    dest: &Dest<'_>,
    entry: &mut Entry,
    key: &str,
    present: bool,
    present_why: &str,
) -> bool {
    let mut ledger = dest.ledger.borrow_mut();
    if ledger.get(key).is_some() {
        writes::unchanged(entry);
        return false;
    }
    let hash = ledger::digest(key.as_bytes());
    ledger.record(key, hash.clone(), hash);
    if present {
        entry.disposition = Disposition::Skipped;
        present_why.clone_into(&mut entry.reason);
    }
    !present
}

/// One text file written whole (an identity file, a workspace
/// `AGENTS.md`).
pub struct FileStep {
    /// The component.
    pub component: Component,
    /// The entries.
    pub entries: Vec<Entry>,
    /// Where it goes: relative to the profile, or absolute.
    pub path: PathBuf,
    /// The text.
    pub content: String,
}

impl Step for FileStep {
    fn component(&self) -> Component {
        self.component
    }

    fn entries(&mut self) -> &mut Vec<Entry> {
        &mut self.entries
    }

    fn apply(&mut self, dest: &Dest<'_>) -> Result<(), Error> {
        let path = dest.scope.dir().join(&self.path);
        let current = std::fs::read_to_string(&path)
            .ok()
            .filter(|t| !t.trim().is_empty())
            .map(|t| ledger::digest(t.as_bytes()));
        let new = ledger::digest(self.content.as_bytes());
        let key = format!("file:{}", self.path.display());
        let prior = dest.ledger.borrow().get(&key).cloned();
        let decision = ledger::decide(
            prior.as_ref(),
            &new,
            &new,
            current.as_deref(),
            dest.overwrite,
        );
        let why = format!("{} already has other content", path.display());
        for e in &mut self.entries {
            writes::settle(decision, e, &why);
        }
        if decision == Decision::Write {
            dest.keep(&path)?;
            write_file(&path, &self.content)?;
        }
        if matches!(decision, Decision::Write | Decision::Same) {
            dest.ledger.borrow_mut().record(key, new.clone(), new);
        }
        Ok(())
    }
}

fn write_file(path: &Path, text: &str) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::io_doing(&codes::HOME_IO, "cannot create", parent, &e))?;
    }
    pmt_core::private_fs::write_atomic(path, text.as_bytes())
        .map_err(|e| Error::io_doing(&codes::HOME_IO, "cannot write", path, &e))
}
