//! What every importer plans with: report entries per component, memory
//! paragraphs, scanned skill copies, MCP servers, config keys, command
//! approvals and whole files, collected into steps in commit order.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use pmt_memory::manage::Item;
use pmt_store::{MemoryKind, MemoryTarget, MemoryTier};
use serde_json::{Map, Value};

use super::read::Reader;
use super::steps::{ApprovalsStep, FileStep, MemoryStep, Notes, SkillsStep, WritesStep};
use super::{Options, Plan, mcp};
use crate::commit::Planned;
use crate::component::Component;
use crate::plan::{Dest, Step};
use crate::report::{Disposition, Entry};
use crate::writes::Writes;

/// A step that fails: a source file that could not be read or parsed
/// fails its component (exit 1) while the rest imports.
struct Broken {
    component: Component,
    entries: Vec<Entry>,
}

impl Step for Broken {
    fn component(&self) -> Component {
        self.component
    }

    fn entries(&mut self) -> &mut Vec<Entry> {
        &mut self.entries
    }

    fn apply(&mut self, _: &Dest<'_>) -> Result<(), Error> {
        let reasons: Vec<&str> = self.entries.iter().map(|e| e.reason.as_str()).collect();
        Err(Error::new(&codes::MIGRATE_COMPONENT, reasons.join("; ")))
    }
}

/// The plan being built.
pub(super) struct Build<'a> {
    pub(super) options: &'a Options,
    pub(super) reader: Reader,
    memory: MemoryStep,
    skills: SkillsStep,
    skill_names: BTreeSet<String>,
    pub(super) mcp: WritesStep,
    pub(super) config: WritesStep,
    approvals: ApprovalsStep,
    files: Vec<FileStep>,
    broken: Vec<Broken>,
    notes: Vec<Entry>,
}

impl<'a> Build<'a> {
    pub(super) fn new(options: &'a Options) -> Self {
        let writes = |component| WritesStep {
            component,
            entries: Vec::new(),
            writes: Writes::default(),
        };
        Self {
            options,
            reader: Reader::new(&options.source),
            memory: MemoryStep {
                entries: Vec::new(),
                files: Vec::new(),
                items: Vec::new(),
                provenance: format!("import:{}", options.agent.id()),
            },
            skills: SkillsStep {
                entries: Vec::new(),
                copies: Vec::new(),
                on_conflict: options.openclaw.skill_conflict,
            },
            skill_names: BTreeSet::new(),
            mcp: writes(Component::Mcp),
            config: writes(Component::Config),
            approvals: ApprovalsStep {
                entries: Vec::new(),
                allow: Vec::new(),
                deny: Vec::new(),
            },
            files: Vec::new(),
            broken: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// The source-relative name of `path` for report items.
    pub(super) fn item(&self, path: &Path) -> String {
        path.strip_prefix(self.reader.root())
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// Records an entry that writes nothing.
    pub(super) fn note(&mut self, entry: Entry) {
        self.notes.push(entry);
    }

    /// A source file that could not be read or parsed.
    pub(super) fn broken(&mut self, component: Component, item: String, why: String) {
        self.broken.push(Broken {
            component,
            entries: vec![Entry::new(component, item, Disposition::Failed).because(why)],
        });
    }

    /// Reads JSON at `path`, recording a broken file under `component`.
    pub(super) fn json(&mut self, component: Component, path: &Path) -> Option<Value> {
        match self.reader.json(path) {
            Ok(v) => v,
            Err(why) => {
                let item = self.item(path);
                self.broken(component, item, why);
                None
            }
        }
    }

    /// Reads the top-level `key` of the JSON file at `path` (only it
    /// counts for the sync digest), recording a broken file under
    /// `component`.
    pub(super) fn json_part(
        &mut self,
        component: Component,
        path: &Path,
        key: &str,
    ) -> Option<Value> {
        match self.reader.json_part(path, key) {
            Ok(v) => v,
            Err(why) => {
                let item = self.item(path);
                self.broken(component, item, why);
                None
            }
        }
    }

    /// Imports the text file at `path` as memory entries of `target`:
    /// its paragraphs, or its `§`-separated entries when `sections`.
    pub(super) fn memory_file(
        &mut self,
        path: &Path,
        target: MemoryTarget,
        kind: MemoryKind,
        sections: bool,
    ) {
        let item = self.item(path);
        let text = match self.reader.text(path) {
            Ok(Some(t)) => t,
            Ok(None) => return,
            Err(why) => return self.broken(Component::Memory, item, why),
        };
        let body = super::read::strip_frontmatter(&text);
        let parts = if sections {
            crate::memory::split(body)
        } else {
            super::read::paragraphs(body)
        };
        let index = self.memory.entries.len();
        self.memory.files.push((index, format!("memory:{item}")));
        self.memory.entries.push(
            Entry::new(Component::Memory, item, Disposition::Mapped)
                .to(match target {
                    MemoryTarget::User => "user memory",
                    MemoryTarget::Memory => "agent memory",
                })
                .because(format!("{} entries", parts.len())),
        );
        self.memory.items.extend(parts.into_iter().map(|content| {
            (
                index,
                Item {
                    id: None,
                    target,
                    kind,
                    tier: MemoryTier::Core,
                    pinned: false,
                    content,
                },
            )
        }));
    }

    /// Imports every skill directory under `dir` (at any depth when
    /// `deep`) to `skills/<agent>-imports/<name>/`; a second skill of a
    /// name already planned is a conflict.
    pub(super) fn skills_in(&mut self, dir: &Path, deep: bool) {
        let prefix = format!("{}-imports", self.options.agent.id());
        for skill in self.reader.skill_dirs(dir, deep) {
            let name = skill
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let item = self.item(&skill);
            if !self.skill_names.insert(name.clone()) {
                self.skills.entries.push(
                    Entry::new(Component::Skills, item, Disposition::Conflict).because(format!(
                        "another {} skill named '{name}' is imported",
                        self.options.agent.label()
                    )),
                );
                continue;
            }
            self.skills.add(item, skill, format!("{prefix}/{name}"));
        }
    }

    /// Plans MCP servers of `servers` (`item` prefixes report items).
    pub(super) fn mcp_servers(
        &mut self,
        item: &str,
        servers: &Map<String, Value>,
        secrets: mcp::Secrets,
    ) {
        mcp::plan(
            item,
            servers,
            secrets,
            &mut self.mcp.entries,
            &mut self.mcp.writes,
        );
    }

    /// A config key to write for a new entry.
    pub(super) fn key(&mut self, entry: Entry, key: &str, value: impl Into<toml_edit::Value>) {
        let index = self.config.entries.len();
        self.config.entries.push(entry);
        self.config.writes.key(key, value, index);
    }

    /// A secret to store for a new entry.
    pub(super) fn secret(&mut self, entry: Entry, name: &str, value: &str) {
        let index = self.config.entries.len();
        self.config.entries.push(entry);
        self.config.writes.secret(name, value, index);
    }

    /// A permanent command approval.
    pub(super) fn allow(&mut self, entry: Entry, pattern: String) {
        self.approvals
            .allow
            .push((self.approvals.entries.len(), pattern));
        self.approvals.entries.push(entry);
    }

    /// A user deny rule.
    pub(super) fn deny(&mut self, entry: Entry, pattern: String) {
        self.approvals
            .deny
            .push((self.approvals.entries.len(), pattern));
        self.approvals.entries.push(entry);
    }

    /// A whole file to write (relative to the profile, or absolute).
    pub(super) fn file(
        &mut self,
        component: Component,
        entry: Entry,
        path: PathBuf,
        content: String,
    ) {
        self.files.push(FileStep {
            component,
            entries: vec![entry],
            path,
            content,
        });
    }

    /// The steps in commit order and the digest of what was read.
    pub(super) fn finish(self) -> Plan {
        let digest = self.reader.digest();
        let mut steps: Vec<Box<dyn Step>> = Vec::new();
        if !self.config.entries.is_empty() {
            steps.push(Box::new(self.config));
        }
        if !self.approvals.entries.is_empty() {
            steps.push(Box::new(self.approvals));
        }
        for f in self.files {
            steps.push(Box::new(f));
        }
        if !self.memory.entries.is_empty() {
            steps.push(Box::new(self.memory));
        }
        if !self.skills.entries.is_empty() {
            steps.push(Box::new(self.skills));
        }
        if !self.mcp.entries.is_empty() {
            steps.push(Box::new(self.mcp));
        }
        for b in self.broken {
            steps.push(Box::new(b));
        }
        let mut by_component: std::collections::BTreeMap<Component, Vec<Entry>> =
            std::collections::BTreeMap::new();
        for e in self.notes {
            by_component.entry(e.component).or_default().push(e);
        }
        for (component, entries) in by_component {
            steps.push(Box::new(Notes { component, entries }));
        }
        let order = |c: Component| Component::COMMIT_ORDER.iter().position(|x| *x == c);
        steps.sort_by_key(|s| order(s.component()));
        let steps: Planned = steps.into_iter().map(|s| (s.component(), Ok(s))).collect();
        Plan { steps, digest }
    }
}
