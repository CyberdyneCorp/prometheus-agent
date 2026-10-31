//! Every skill the roots define, resolved through the one precedence order
//! (spec: skills, Search roots and single precedence order; SKILL.md
//! format; Skill lookup and wrong-profile hints).
//!
//! The first tier that defines a name wins and later copies are
//! *shadowed*; two different skills sharing a name within one tier make
//! the name *ambiguous* and neither is used. A `SKILL.md` that does not
//! parse is a *problem* (reported by doctor, listed nowhere), and a trusted
//! project skill whose scan is `dangerous` or fails is *quarantined*.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::frontmatter::{self, Document};
use crate::layout::{self, SKILL_FILE};
use crate::lockfile::{Contents, Trust};
use crate::meta::Meta;
use crate::roots::{Root, RootDir, Roots, Tier};
use crate::scan::{Scanner, Verdict};
use crate::source::SkillDir;

/// Builds the directory of a skill found at a relative path of a root.
type DirMaker = Box<dyn Fn(&str) -> SkillDir>;

/// Category of a skill with neither a `category` field nor a parent
/// directory.
pub const DEFAULT_CATEGORY: &str = "general";

/// Where a skill's content came from, for privileges that depend on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// Bundled, or ejected from the bundled tree.
    Builtin,
    /// Written by the user or the agent (profile, project, external,
    /// plugin directories).
    Local,
    /// Installed from a hub at this trust level.
    Hub(Trust),
    /// The lockfile could not be read or names no trust level.
    Unreadable,
}

impl Provenance {
    /// Whether inline shell may expand: never for community skills or
    /// unreadable provenance.
    #[must_use]
    pub const fn allows_inline_shell(self) -> bool {
        !matches!(self, Self::Hub(Trust::Community) | Self::Unreadable)
    }

    /// Whether the skill was installed from a hub.
    #[must_use]
    pub const fn is_hub(self) -> bool {
        matches!(self, Self::Hub(_))
    }
}

/// One skill.
#[derive(Debug, Clone)]
pub struct Skill {
    /// Frontmatter `name` (`<plugin>:<name>` for plugin skills).
    pub name: String,
    /// Frontmatter `description`.
    pub description: String,
    /// `category`, else the first directory below the root, else
    /// [`DEFAULT_CATEGORY`].
    pub category: String,
    /// Its tier.
    pub tier: Tier,
    /// The skill directory relative to its root (`mlops/axolotl`).
    pub rel: String,
    /// The skill directory's own name.
    pub dir_name: String,
    /// Its files.
    pub dir: SkillDir,
    /// The frontmatter as written.
    pub front: Map<String, Value>,
    /// Typed Prometheus fields.
    pub meta: Meta,
    /// The Markdown body (unprocessed).
    pub body: String,
    /// Where it came from.
    pub provenance: Provenance,
}

/// A `SKILL.md` that was not loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// The file (or skill directory).
    pub path: String,
    /// 1-based line (0 when not about a line).
    pub line: usize,
    /// Why.
    pub reason: String,
}

/// Why a lookup found nothing usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Missing {
    /// Nothing has that name.
    NotFound,
    /// `Ambiguous skill name '<name>'` with the paths.
    Ambiguous(Vec<String>),
}

impl Missing {
    /// The ambiguity message, when ambiguous.
    #[must_use]
    pub fn ambiguous_message(name: &str, paths: &[String]) -> String {
        format!("Ambiguous skill name '{name}': {}", paths.join(", "))
    }
}

/// The resolved skills of a set of roots.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    /// The winning skills, in precedence order.
    pub skills: Vec<Arc<Skill>>,
    /// Copies hidden by a higher tier.
    pub shadowed: Vec<Arc<Skill>>,
    /// Names claimed twice within one tier, with their paths.
    pub ambiguous: BTreeMap<String, Vec<String>>,
    /// Unparsable `SKILL.md` files and same-tier collisions.
    pub problems: Vec<Problem>,
    /// Trusted project skills refused by the scan, with the reason.
    pub quarantined: Vec<(Arc<Skill>, String)>,
    /// Notices for the user (from the roots).
    pub notices: Vec<String>,
}

impl Catalog {
    /// Scans `roots`; `lock` gives profile skills their provenance and
    /// `scanner` screens trusted project skills.
    #[must_use]
    pub fn load(roots: &Roots, lock: &Contents, scanner: &dyn Scanner) -> Self {
        let mut catalog = Self {
            notices: roots.notices.clone(),
            ..Self::default()
        };
        let mut by_tier: Vec<(Tier, Vec<Skill>)> = Vec::new();
        for root in &roots.roots {
            let mut skills = catalog.load_root(root, lock);
            if root.tier == Tier::Project {
                skills = catalog.screen(skills, scanner);
            }
            match by_tier.last_mut() {
                Some((tier, list)) if *tier == root.tier => list.extend(skills),
                _ => by_tier.push((root.tier, skills)),
            }
        }
        for (_, skills) in by_tier {
            catalog.add_tier(skills);
        }
        catalog
    }

    fn load_root(&mut self, root: &Root, lock: &Contents) -> Vec<Skill> {
        let (files, make_dir): (Vec<String>, DirMaker) = match &root.dir {
            RootDir::Disk(path) => {
                let base = path.clone();
                (
                    layout::walk(path),
                    Box::new(move |rel| SkillDir::Disk(join(&base, rel))),
                )
            }
            RootDir::Bundled(tree) => {
                let tree = Arc::clone(tree);
                (
                    tree.paths().map(str::to_owned).collect(),
                    Box::new(move |rel| SkillDir::Bundled {
                        tree: Arc::clone(&tree),
                        prefix: rel.to_owned(),
                    }),
                )
            }
        };
        layout::skill_dirs(files.iter().map(String::as_str))
            .into_iter()
            .filter_map(|rel| self.load_skill(root, lock, &rel, make_dir(&rel)))
            .collect()
    }

    fn load_skill(
        &mut self,
        root: &Root,
        lock: &Contents,
        rel: &str,
        dir: SkillDir,
    ) -> Option<Skill> {
        let path = format!("{}/{SKILL_FILE}", dir.display());
        let text = match dir.read(SKILL_FILE) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) => {
                self.problem(path, 0, e.to_string());
                return None;
            }
        };
        let doc = match frontmatter::parse(&text) {
            Ok(doc) => doc,
            Err(e) => {
                self.problem(path, e.line, e.reason);
                return None;
            }
        };
        Some(skill(root, lock, rel, dir, doc))
    }

    fn problem(&mut self, path: String, line: usize, reason: String) {
        tracing::warn!(path = %path, line, reason = %reason, "skill not loaded");
        self.problems.push(Problem { path, line, reason });
    }

    /// Quarantines trusted project skills the scan refuses or fails on.
    fn screen(&mut self, skills: Vec<Skill>, scanner: &dyn Scanner) -> Vec<Skill> {
        let mut kept = Vec::new();
        for skill in skills {
            match scan_skill(&skill, scanner) {
                Ok(()) => kept.push(skill),
                Err(why) => {
                    tracing::warn!(skill = %skill.name, reason = %why, "project skill quarantined");
                    self.quarantined.push((Arc::new(skill), why));
                }
            }
        }
        kept
    }

    fn add_tier(&mut self, skills: Vec<Skill>) {
        let mut names: BTreeMap<String, Vec<Skill>> = BTreeMap::new();
        for skill in skills {
            names.entry(skill.name.clone()).or_default().push(skill);
        }
        for (name, mut group) in names {
            let taken = self.ambiguous.contains_key(&name) || self.find(&name).is_some();
            if taken {
                self.shadowed.extend(group.into_iter().map(Arc::new));
            } else if group.len() > 1 {
                let paths: Vec<String> = group.iter().map(|s| s.dir.display()).collect();
                let reason = Missing::ambiguous_message(&name, &paths);
                self.problem(paths.join(", "), 0, reason);
                self.ambiguous.insert(name, paths);
            } else if let Some(skill) = group.pop() {
                self.skills.push(Arc::new(skill));
            }
        }
    }

    /// The winning skill named exactly `name`.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<&Arc<Skill>> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// Resolves `query`: an ambiguous name, then the directory name, then
    /// the categorized relative path (`mlops/axolotl`), then the
    /// frontmatter name; `<plugin>:<skill>` names only plugin skills.
    ///
    /// # Errors
    ///
    /// [`Missing`].
    pub fn lookup(&self, query: &str) -> Result<Arc<Skill>, Missing> {
        let query = query.trim();
        if let Some(paths) = self.ambiguous.get(query) {
            return Err(Missing::Ambiguous(paths.clone()));
        }
        if query.contains(':') {
            return self.find(query).cloned().ok_or(Missing::NotFound);
        }
        let by_dir: Vec<&Arc<Skill>> = self
            .skills
            .iter()
            .filter(|s| s.tier != Tier::Plugin && s.dir_name == query)
            .collect();
        match by_dir.as_slice() {
            [one] => return Ok(Arc::clone(one)),
            [] => {}
            many => {
                return Err(Missing::Ambiguous(
                    many.iter().map(|s| s.dir.display()).collect(),
                ));
            }
        }
        self.skills
            .iter()
            .find(|s| s.rel == query)
            .or_else(|| self.find(query))
            .cloned()
            .ok_or(Missing::NotFound)
    }
}

fn join(base: &std::path::Path, rel: &str) -> std::path::PathBuf {
    rel.split('/')
        .filter(|c| !c.is_empty())
        .fold(base.to_path_buf(), |path, c| path.join(c))
}

fn skill(root: &Root, lock: &Contents, rel: &str, dir: SkillDir, doc: Document) -> Skill {
    let meta = Meta::of(&doc.front);
    let dir_name = rel.rsplit('/').next().unwrap_or_default().to_owned();
    let category = meta
        .category
        .clone()
        .or_else(|| rel.split_once('/').map(|(first, _)| first.to_owned()))
        .unwrap_or_else(|| DEFAULT_CATEGORY.to_owned());
    let name = match &root.plugin {
        Some(plugin) => format!("{plugin}:{}", doc.name),
        None => doc.name,
    };
    let provenance = provenance(root.tier, lock, &name);
    Skill {
        name,
        description: doc.description,
        category,
        tier: root.tier,
        rel: rel.to_owned(),
        dir_name,
        dir,
        front: doc.front,
        meta,
        body: doc.body,
        provenance,
    }
}

fn provenance(tier: Tier, lock: &Contents, name: &str) -> Provenance {
    match tier {
        Tier::Bundled => Provenance::Builtin,
        Tier::Profile => match lock.entry(name) {
            Err(_) => Provenance::Unreadable,
            Ok(None) => Provenance::Local,
            Ok(Some(entry)) if entry.bundled_path().is_some() => Provenance::Builtin,
            Ok(Some(entry)) => entry.trust.map_or(Provenance::Unreadable, Provenance::Hub),
        },
        Tier::Project | Tier::External | Tier::Plugin => Provenance::Local,
    }
}

/// Every file of `skill` for a scan.
#[must_use]
pub fn files_of(skill: &Skill) -> Vec<(String, Vec<u8>)> {
    skill
        .dir
        .files()
        .into_iter()
        .filter_map(|rel| skill.dir.read(&rel).ok().map(|bytes| (rel, bytes)))
        .collect()
}

/// `Ok` when the scan passes; the refusal reason otherwise (a `dangerous`
/// verdict or a scanner failure).
fn scan_skill(skill: &Skill, scanner: &dyn Scanner) -> Result<(), String> {
    match scanner.scan(&files_of(skill)) {
        Ok(report) if report.verdict == Verdict::Dangerous => {
            Err(format!("security scan: {}", report.summary()))
        }
        Ok(_) => Ok(()),
        Err(e) => Err(format!("security scan failed: {e}")),
    }
}
