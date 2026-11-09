//! Named subagent definitions (spec: subagent-delegation, Named subagent
//! definitions): Markdown files `<profile home>/agents/<name>.md` (and the
//! active harness's `agents/`) whose front matter names and describes the
//! agent and whose body becomes the child's system prompt. A session
//! freezes the set it starts with ([`Definitions`]), so a file added
//! mid-session is unknown to it until the next session.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Longest definition description.
pub const MAX_DESCRIPTION_CHARS: usize = 600;

/// One definition.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentDefinition {
    /// `name` (equal to the file stem).
    pub name: String,
    /// `description`.
    pub description: String,
    /// `toolsets` narrowing.
    pub toolsets: Option<Vec<String>>,
    /// `model` (`provider/model`).
    pub model: Option<String>,
    /// `max_iterations`.
    pub max_iterations: Option<u32>,
    /// `output_schema`.
    pub output_schema: Option<Value>,
    /// `background`: whether its spawns may run in the background.
    pub background: Option<bool>,
    /// The Markdown body: the child's system prompt.
    pub prompt: String,
}

/// A file that was not loaded, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidDefinition {
    /// The file.
    pub path: PathBuf,
    /// What is wrong.
    pub reason: String,
}

/// The definitions a session froze at its start.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Definitions {
    /// Loaded definitions, by name (first directory wins).
    pub agents: Vec<AgentDefinition>,
    /// Files that failed to load.
    pub invalid: Vec<InvalidDefinition>,
}

impl Definitions {
    /// Loads every `*.md` of `dirs` (missing directories are empty).
    #[must_use]
    pub fn load(dirs: &[PathBuf]) -> Self {
        let mut out = Self::default();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut paths: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "md"))
                .collect();
            paths.sort();
            for path in paths {
                match read(&path) {
                    Ok(def) if out.get(&def.name).is_none() => out.agents.push(def),
                    Ok(_) => {}
                    Err(reason) => {
                        tracing::warn!(path = %path.display(), %reason, "subagent definition not loaded");
                        out.invalid.push(InvalidDefinition { path, reason });
                    }
                }
            }
        }
        out
    }

    /// The definition named `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&AgentDefinition> {
        self.agents.iter().find(|a| a.name == name)
    }

    /// The defined names.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.agents.iter().map(|a| a.name.clone()).collect()
    }

    /// The `delegate_task` description lines listing every definition.
    #[must_use]
    pub fn listing(&self) -> Option<String> {
        if self.agents.is_empty() {
            return None;
        }
        let lines: Vec<String> = self
            .agents
            .iter()
            .map(|a| format!("- {}: {}", a.name, a.description))
            .collect();
        Some(format!(
            "Named agents (pass as `agent`):\n{}",
            lines.join("\n")
        ))
    }
}

/// Reads one definition file.
fn read(path: &Path) -> Result<AgentDefinition, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("unreadable: {e}"))?;
    let doc = pmt_skills::frontmatter::parse(&text)
        .map_err(|e| format!("line {}: {}", e.line, e.reason))?;
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if doc.name != stem {
        return Err(format!(
            "front matter name '{}' does not match the file name '{stem}'",
            doc.name
        ));
    }
    if doc.description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(format!(
            "description is longer than {MAX_DESCRIPTION_CHARS} characters"
        ));
    }
    let front = &doc.front;
    let toolsets = match front.get("toolsets") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .map(|v| v.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .ok_or("toolsets must list strings")?,
        ),
        Some(_) => return Err("toolsets must be a list".to_owned()),
    };
    let max_iterations = match front.get("max_iterations") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| (1..=1000).contains(n))
                .ok_or("max_iterations must be an integer from 1 to 1000")?,
        ),
    };
    let output_schema = front.get("output_schema").filter(|v| !v.is_null()).cloned();
    if let Some(schema) = &output_schema {
        crate::schema_check::check(schema).map_err(|e| format!("output_schema: {e}"))?;
    }
    Ok(AgentDefinition {
        name: doc.name,
        description: doc.description,
        toolsets,
        model: front
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_owned),
        max_iterations,
        output_schema,
        background: front.get("background").and_then(Value::as_bool),
        prompt: doc.body.trim().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) {
        std::fs::write(dir.join(format!("{name}.md")), text).unwrap();
    }

    /// Scenario: Mismatched name rejected at load.
    #[test]
    fn a_name_that_differs_from_the_file_registers_neither() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "reviewer",
            "---\nname: critic\ndescription: Reviews code\n---\nBe strict.\n",
        );
        let defs = Definitions::load(&[dir.path().to_path_buf()]);
        assert!(defs.agents.is_empty());
        let reason = &defs.invalid[0].reason;
        assert!(
            reason.contains("critic") && reason.contains("reviewer"),
            "{reason}"
        );
    }

    #[test]
    fn definitions_load_with_their_fields_and_are_listed() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "reviewer",
            "---\nname: reviewer\ndescription: Reviews diffs for bugs\ntoolsets: [file]\nmodel: mock/review\nmax_iterations: 12\n---\nYou review code.\n",
        );
        let defs = Definitions::load(&[dir.path().to_path_buf()]);
        let def = defs.get("reviewer").unwrap();
        assert_eq!(def.toolsets.as_deref(), Some(&["file".to_owned()][..]));
        assert_eq!(def.model.as_deref(), Some("mock/review"));
        assert_eq!(def.max_iterations, Some(12));
        assert_eq!(def.prompt, "You review code.");
        assert!(
            defs.listing()
                .unwrap()
                .contains("- reviewer: Reviews diffs for bugs")
        );
    }

    #[test]
    fn a_long_description_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let long = "d".repeat(601);
        write(
            dir.path(),
            "a",
            &format!("---\nname: a\ndescription: {long}\n---\nx\n"),
        );
        assert!(
            Definitions::load(&[dir.path().to_path_buf()])
                .agents
                .is_empty()
        );
    }
}
