//! The `memory` component (spec: hermes-migration, Memory and identity
//! imported): `memories/MEMORY.md` and `memories/USER.md` split on the
//! Hermes entry delimiter `\n§\n` into ordered entries of the agent and
//! user stores. Every entry is imported even past the Prometheus budget
//! (reported, `memory curate` trims it). An external memory provider is
//! carried only when a Prometheus provider plugin of that name is
//! installed.

use pmt_core::error::Error;
use pmt_memory::manage::{self, Item, Limits};
use pmt_store::{MemoryKind, MemoryTarget, MemoryTier};
use serde_json::Value;

use crate::component::Component;
use crate::host::Registry;
use crate::ledger;
use crate::plan::{Dest, Source, Step};
use crate::report::{Disposition, Entry};
use crate::writes;

/// The Hermes entry delimiter.
pub const DELIMITER: &str = "\n§\n";

/// The planned memory component.
pub struct MemoryStep {
    entries: Vec<Entry>,
    items: Vec<(usize, Item)>,
    /// Entry index, ledger key and content hash of each memory file.
    files: Vec<(usize, String, String)>,
    provider: Option<(usize, String)>,
}

impl Step for MemoryStep {
    fn component(&self) -> Component {
        Component::Memory
    }

    fn entries(&mut self) -> &mut Vec<Entry> {
        &mut self.entries
    }

    fn apply(&mut self, dest: &Dest<'_>) -> Result<(), Error> {
        if let Some((index, name)) = &self.provider {
            if dest
                .host
                .known(Registry::MemoryProvider, name, Some(dest.scope))
            {
                let mut writes = crate::writes::Writes::default();
                writes.key("memory.provider", name.as_str(), *index);
                writes.apply(dest, &mut self.entries)?;
            } else {
                let e = &mut self.entries[*index];
                e.disposition = Disposition::Unsupported;
                e.reason = format!("no Prometheus memory provider plugin '{name}' is installed");
            }
        }
        // A file unchanged since the last run is not imported again (the
        // import deduplicates anyway; this keeps entries the user removed
        // removed).
        let mut fresh = Vec::new();
        for (index, key, hash) in &self.files {
            if dest
                .ledger
                .borrow()
                .get(key)
                .is_some_and(|r| r.hash == *hash)
            {
                writes::unchanged(&mut self.entries[*index]);
            } else {
                fresh.push(*index);
            }
        }
        let items: Vec<Item> = self
            .items
            .iter()
            .filter(|(index, _)| fresh.contains(index))
            .map(|(_, i)| i.clone())
            .collect();
        if !items.is_empty() {
            self.import(dest, &items)?;
        }
        let mut ledger = dest.ledger.borrow_mut();
        for (index, key, hash) in &self.files {
            if fresh.contains(index) {
                ledger.record(key.clone(), hash.clone(), hash.clone());
            }
        }
        Ok(())
    }
}

impl MemoryStep {
    fn import(&mut self, dest: &Dest<'_>, items: &[Item]) -> Result<(), Error> {
        let redactor = dest.scope.secrets().redactor()?;
        let summary = manage::import_with(
            dest.store,
            &dest.scope.config().memory,
            items,
            &redactor,
            Limits::Exceed,
        )?;
        if summary.over_budget {
            for (index, _) in &self.items {
                let e = &mut self.entries[*index];
                if !e.reason.contains("over budget") {
                    e.reason
                        .push_str("; over budget; run prometheus memory curate");
                }
            }
        }
        Ok(())
    }
}

/// The entries of a Hermes memory file, in order.
#[must_use]
pub fn split(text: &str) -> Vec<String> {
    text.replace("\r\n", "\n")
        .split(DELIMITER)
        .map(str::trim)
        .filter(|e| !e.is_empty() && *e != "§")
        .map(str::to_owned)
        .collect()
}

/// Plans the memory component.
///
/// # Errors
///
/// `migrate.component` when a memory file exists but cannot be read.
pub fn plan(src: &Source<'_>) -> Result<MemoryStep, Error> {
    let mut step = MemoryStep {
        entries: Vec::new(),
        items: Vec::new(),
        files: Vec::new(),
        provider: None,
    };
    for (file, target) in [
        ("memories/MEMORY.md", MemoryTarget::Memory),
        ("memories/USER.md", MemoryTarget::User),
    ] {
        let Some(text) = src.profile.read(file)? else {
            continue;
        };
        let parts = split(&text);
        let index = step.entries.len();
        step.files.push((
            index,
            format!("memory:{file}"),
            ledger::digest(text.as_bytes()),
        ));
        step.entries.push(
            Entry::new(Component::Memory, file, Disposition::Mapped)
                .to(format!("{target} memory"))
                .because(format!("{} entries", parts.len())),
        );
        step.items.extend(parts.into_iter().map(|content| {
            (
                index,
                Item {
                    id: None,
                    target,
                    kind: MemoryKind::Fact,
                    tier: MemoryTier::Core,
                    pinned: false,
                    content,
                },
            )
        }));
    }
    if let Some(provider) = src.get("memory.provider").and_then(Value::as_str)
        && !provider.trim().is_empty()
    {
        step.provider = Some((step.entries.len(), provider.trim().to_owned()));
        step.entries.push(
            Entry::new(Component::Memory, "memory.provider", Disposition::Mapped)
                .to("memory.provider")
                .showing(provider),
        );
    }
    Ok(step)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_split_on_the_section_sign() {
        assert_eq!(
            split("first\n§\nsecond line\nmore\n§\nthird\n"),
            ["first", "second line\nmore", "third"]
        );
        assert!(split("").is_empty());
    }
}
