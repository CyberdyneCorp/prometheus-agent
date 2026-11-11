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
    /// Entry index and ledger key of each memory file.
    files: Vec<(usize, String)>,
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
        let fresh = Fresh::of(dest, &self.files, &self.items, &mut self.entries);
        fresh.import(dest, |items| self.import(dest, items))
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

/// The memory entries of a run that no earlier run imported (spec:
/// hermes-migration, Idempotent incremental re-run; Keeping imports in
/// sync). Each entry is recorded in the ledger as
/// `<file key>#<digest of its text>`, so one imported before is never
/// imported again, even after the user edited or removed it in
/// Prometheus, while a new entry of a changed file still is.
pub(crate) struct Fresh {
    items: Vec<Item>,
    keys: Vec<(String, String)>,
}

impl Fresh {
    /// The new items among `items` (entry index of their file, item),
    /// whose files are `files` (entry index, ledger key); the entry of a
    /// file with nothing new becomes `unchanged`.
    pub(crate) fn of(
        dest: &Dest<'_>,
        files: &[(usize, String)],
        items: &[(usize, Item)],
        entries: &mut [Entry],
    ) -> Self {
        let ledger = dest.ledger.borrow();
        let mut fresh = Self {
            items: Vec::new(),
            keys: Vec::new(),
        };
        for (index, file) in files {
            let mut known = 0;
            for (_, item) in items.iter().filter(|(i, _)| i == index) {
                let hash = ledger::digest(item.content.as_bytes());
                let key = format!("{file}#{hash}");
                if ledger.get(&key).is_some() {
                    known += 1;
                } else if !fresh.keys.iter().any(|(k, _)| *k == key) {
                    fresh.items.push(item.clone());
                    fresh.keys.push((key, hash));
                }
            }
            let e = &mut entries[*index];
            let total = items.iter().filter(|(i, _)| i == index).count();
            if known > 0 && known == total {
                writes::unchanged(e);
            } else if known > 0 {
                e.reason = format!("{} new entries; {known} imported before", total - known);
            }
        }
        fresh
    }

    /// Imports the new items through `import` (one memory transaction),
    /// records them, and how to take the import back.
    ///
    /// # Errors
    ///
    /// `import`'s error; store errors.
    pub(crate) fn import(
        self,
        dest: &Dest<'_>,
        import: impl FnOnce(&[Item]) -> Result<(), Error>,
    ) -> Result<(), Error> {
        if self.items.is_empty() {
            return Ok(());
        }
        let before = dest.store.memory_entries(None)?;
        import(&self.items)?;
        dest.on_undo(move |store| {
            store.memory_batch(None, |batch| {
                batch.entries.clone_from(&before);
                Ok(())
            })
        });
        let mut ledger = dest.ledger.borrow_mut();
        for (key, hash) in self.keys {
            ledger.record(key, hash.clone(), hash);
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
        step.files.push((index, format!("memory:{file}")));
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
