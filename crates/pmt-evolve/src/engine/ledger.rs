//! The budget ledger (spec: harness-evolution, Budget ledger and spend
//! cap; Concurrency limits). Each mode lists its rollouts and improver
//! sessions in the order a serial run would produce them, whatever order
//! they finished in; at mode completion the budget identity (R = 4k,
//! P = 3k, no label repeated within a kind) is asserted. Selection,
//! reflection and domain classification costs are kept apart from the
//! identity.

use serde::{Deserialize, Serialize};

/// What a ledger entry paid for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A rollout.
    Rollout,
    /// An improver session (a stage, composition or checkpoint).
    Session,
    /// The selector session.
    Selection,
    /// The reflection session.
    Reflection,
    /// The domain classification at job start.
    Classification,
}

/// One ledger entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// What it paid for.
    pub kind: Kind,
    /// Its label.
    pub label: String,
    /// US dollars (0 for a cache hit).
    pub usd: f64,
    /// Served from the content-addressed cache.
    pub cached: bool,
    /// Attempts it took.
    pub attempts: u32,
}

/// One mode's ledger.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModeLedger {
    /// `parallel` or `sequential`.
    pub mode: String,
    /// Rollouts and sessions, in serial order.
    pub entries: Vec<Entry>,
    /// The selection, when one ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<Entry>,
    /// The reflection, when one ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reflection: Option<Entry>,
}

impl ModeLedger {
    /// An empty ledger of `mode`.
    #[must_use]
    pub fn new(mode: &str) -> Self {
        Self {
            mode: mode.to_owned(),
            ..Self::default()
        }
    }

    /// Appends an entry.
    pub fn push(&mut self, entry: Entry) {
        self.entries.push(entry);
    }

    /// Entries of `kind`.
    pub fn of(&self, kind: Kind) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(move |e| e.kind == kind)
    }

    /// Asserts R = 4k rollouts, P = 3k sessions and unique labels per
    /// kind.
    ///
    /// # Errors
    ///
    /// `budget identity violated` with what is off.
    pub fn check(&self, k: u8) -> Result<(), String> {
        let k = usize::from(k);
        let rollouts: Vec<&str> = self.of(Kind::Rollout).map(|e| e.label.as_str()).collect();
        let sessions: Vec<&str> = self.of(Kind::Session).map(|e| e.label.as_str()).collect();
        let unique = |labels: &[&str]| {
            let mut sorted = labels.to_vec();
            sorted.sort_unstable();
            sorted.dedup();
            sorted.len() == labels.len()
        };
        if rollouts.len() != 4 * k
            || sessions.len() != 3 * k
            || !unique(&rollouts)
            || !unique(&sessions)
        {
            return Err(format!(
                "budget identity violated ({} rollouts, {} sessions at k={k})",
                rollouts.len(),
                sessions.len()
            ));
        }
        Ok(())
    }
}

/// Spend by kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Costs {
    /// Rollouts.
    pub rollout: f64,
    /// Propose and compose sessions.
    pub improver: f64,
    /// Selector sessions.
    pub selection: f64,
    /// Reflection sessions.
    pub reflection: f64,
    /// The domain classification (outside the R/P identity).
    #[serde(default)]
    pub classification: f64,
}

impl Costs {
    /// Everything.
    #[must_use]
    pub fn total(&self) -> f64 {
        self.rollout + self.improver + self.selection + self.reflection + self.classification
    }

    /// Adds `usd` under `kind`.
    pub fn add(&mut self, kind: Kind, usd: f64) {
        match kind {
            Kind::Rollout => self.rollout += usd,
            Kind::Session => self.improver += usd,
            Kind::Selection => self.selection += usd,
            Kind::Reflection => self.reflection += usd,
            Kind::Classification => self.classification += usd,
        }
    }
}

/// `ledger.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Ledger {
    /// One ledger per mode, in run order.
    pub modes: Vec<ModeLedger>,
    /// Spend by kind across the job (shared base rollouts once).
    pub costs: Costs,
    /// [`Costs::total`].
    pub total_usd: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: Kind, label: &str) -> Entry {
        Entry {
            kind,
            label: label.to_owned(),
            usd: 0.0,
            cached: false,
            attempts: 1,
        }
    }

    #[test]
    fn the_identity_holds_only_for_four_k_rollouts_and_three_k_sessions() {
        let mut ledger = ModeLedger::new("parallel");
        for label in ["base1", "inv1", "dom1", "agg1"] {
            ledger.push(entry(Kind::Rollout, label));
        }
        for label in ["inv1", "dom1", "agg1"] {
            ledger.push(entry(Kind::Session, label));
        }
        assert!(ledger.check(1).is_ok());
        ledger.entries[3].label = "inv1".into();
        assert!(
            ledger
                .check(1)
                .unwrap_err()
                .starts_with("budget identity violated")
        );
        assert!(ledger.check(2).is_err());
    }
}
