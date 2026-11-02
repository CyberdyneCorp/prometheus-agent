//! The scenarios of a milestone, read from the capability specs: every
//! `#### Scenario:` under a `### Requirement:` whose `**Milestone:**` line
//! names the milestone.

use std::path::Path;

/// One scenario of a requirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Scenario {
    pub milestone: String,
    pub capability: String,
    pub requirement: String,
    pub name: String,
}

impl Scenario {
    pub(crate) fn key(&self) -> String {
        key(&self.capability, &self.requirement, &self.name)
    }
}

/// How the index and the messages name a scenario.
pub(crate) fn key(capability: &str, requirement: &str, scenario: &str) -> String {
    format!("{capability} › {requirement} › {scenario}")
}

/// The `milestone` scenarios of `capabilities` under `dir/<capability>/spec.md`.
pub(crate) fn collect(
    dir: &Path,
    capabilities: &[String],
    milestone: &str,
) -> Result<Vec<Scenario>, String> {
    let mut all = Vec::new();
    for capability in capabilities {
        let path = dir.join(capability).join("spec.md");
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        all.extend(parse(capability, &text, milestone));
    }
    Ok(all)
}

/// The `milestone` scenarios of one spec's text.
pub(crate) fn parse(capability: &str, text: &str, milestone: &str) -> Vec<Scenario> {
    let mut found = Vec::new();
    for block in text.split("\n### Requirement: ").skip(1) {
        let requirement = block.lines().next().unwrap_or_default().trim();
        if milestone_of(block) != Some(milestone) {
            continue;
        }
        found.extend(block.lines().filter_map(|line| {
            line.strip_prefix("#### Scenario: ").map(|name| Scenario {
                milestone: milestone.to_owned(),
                capability: capability.to_owned(),
                requirement: requirement.to_owned(),
                name: name.trim().to_owned(),
            })
        }));
    }
    found
}

fn milestone_of(block: &str) -> Option<&str> {
    block
        .lines()
        .find_map(|line| line.trim().strip_prefix("**Milestone:**"))
        .and_then(|rest| rest.split_whitespace().next())
}
