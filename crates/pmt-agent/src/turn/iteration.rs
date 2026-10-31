//! The per-agent iteration budget (spec: agent-turn-loop, Single
//! documented iteration limit; Per-agent iteration budget).
//!
//! Every agent (root or delegated child) owns one: it is never shared
//! with a parent or sibling, `remaining` never drops below 0, a refund
//! never drops `used` below 0, and `max = 0` means unlimited.

/// Model calls allowed and made in one turn of one agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IterationBudget {
    max: u32,
    used: u32,
}

impl IterationBudget {
    /// A budget of `max` calls (`agent.max_iterations`; 0 = unlimited).
    #[must_use]
    pub const fn new(max: u32) -> Self {
        Self { max, used: 0 }
    }

    /// Takes one iteration; `false` (nothing taken) when none remain.
    pub fn consume(&mut self) -> bool {
        if self.max != 0 && self.used >= self.max {
            return false;
        }
        self.used = self.used.saturating_add(1);
        true
    }

    /// Gives back one iteration (a compression attempt that never reached
    /// the provider).
    pub fn refund(&mut self) {
        self.used = self.used.saturating_sub(1);
    }

    /// Iterations taken.
    #[must_use]
    pub const fn used(&self) -> u32 {
        self.used
    }

    /// Iterations left; `None` when unlimited.
    #[must_use]
    pub const fn remaining(&self) -> Option<u32> {
        if self.max == 0 {
            None
        } else {
            Some(self.max.saturating_sub(self.used))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_and_used_stay_in_range() {
        let mut budget = IterationBudget::new(2);
        assert!(budget.consume() && budget.consume());
        assert!(!budget.consume());
        assert_eq!((budget.used(), budget.remaining()), (2, Some(0)));
        budget.refund();
        budget.refund();
        budget.refund();
        assert_eq!((budget.used(), budget.remaining()), (0, Some(2)));
    }

    #[test]
    fn zero_is_unlimited() {
        let mut budget = IterationBudget::new(0);
        for _ in 0..10_000 {
            assert!(budget.consume());
        }
        assert_eq!(budget.remaining(), None);
    }

    #[test]
    fn a_child_budget_is_its_own() {
        let mut parent = IterationBudget::new(400);
        let mut child = IterationBudget::new(50);
        while child.consume() {}
        assert_eq!(child.used(), 50);
        assert_eq!(parent.remaining(), Some(400));
        assert!(parent.consume());
    }
}
