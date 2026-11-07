//! The budget checkpoint notice (spec: agent-turn-loop, Budget checkpoint
//! notice).
//!
//! With `agent.budget_warning_ratio` set, the first tool round of a turn in
//! which the most-constrained budget (iterations, the run budget, or any
//! applicable `budgets.*` limit) has crossed that ratio appends
//! `[budget checkpoint] used <used> of <max> <unit>` to its newest result,
//! which is not yet sent. Unlimited budgets never count, and the notice
//! never goes into the system prompt or an already-sent row.

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_obs::budget::{Place, Unit};
use pmt_tools::Content;

use super::run::Turn;
use super::tools::ToolOutcome;

/// One budget's use.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Use {
    pub(super) used: f64,
    pub(super) max: f64,
    pub(super) unit: &'static str,
}

impl Use {
    fn ratio(&self) -> f64 {
        self.used / self.max
    }

    /// `[budget checkpoint] used 15 of 20 iterations`.
    pub(super) fn notice(&self) -> String {
        let show = |v: f64| {
            if self.unit == "USD" {
                format!("{v:.2}")
            } else {
                format!("{v:.0}")
            }
        };
        format!(
            "[budget checkpoint] used {} of {} {}",
            show(self.used),
            show(self.max),
            self.unit
        )
    }
}

/// The use with the highest ratio, when it reached `ratio`.
pub(super) fn crossed(uses: Vec<Use>, ratio: f64) -> Option<Use> {
    uses.into_iter()
        .filter(|u| u.max > 0.0)
        .max_by(|a, b| a.ratio().total_cmp(&b.ratio()))
        .filter(|u| u.ratio() >= ratio)
}

impl Turn<'_> {
    /// Appends the notice to the newest outcome once per turn.
    pub(super) fn checkpoint(&mut self, outcomes: &mut [ToolOutcome]) -> Result<(), Error> {
        let Some(ratio) = self.config.agent.budget_warning_ratio else {
            return Ok(());
        };
        if self.checkpoint_sent || outcomes.is_empty() {
            return Ok(());
        }
        let Some(crossed) = crossed(self.budget_uses()?, ratio) else {
            return Ok(());
        };
        if let Some(last) = outcomes.last_mut() {
            last.result.content.push(Content::Text {
                text: crossed.notice(),
            });
            self.checkpoint_sent = true;
        }
        Ok(())
    }

    /// Every limited budget of this turn with its current use.
    fn budget_uses(&self) -> Result<Vec<Use>, Error> {
        let mut uses = Vec::new();
        if let Some(max) = self.iterations.max() {
            uses.push(Use {
                used: f64::from(self.iterations.used()),
                max: f64::from(max),
                unit: "iterations",
            });
        }
        if let Some(clock) = self.clock {
            uses.push(Use {
                used: clock.started.elapsed().as_secs_f64().floor(),
                max: clock.budget.as_secs_f64(),
                unit: "seconds",
            });
        }
        let setup = &self.agent.setup;
        let place = Place::of(&setup.store, setup.session, Some(self.started_at))?;
        let status = self.agent.budgets.status(
            &setup.store,
            &self.config.budgets,
            &place,
            setup.unattended,
            Timestamp::now(),
        )?;
        uses.extend(status.into_iter().map(|(limit, spent)| Use {
            used: spent,
            max: limit.limit,
            unit: match limit.unit {
                Unit::Usd => "USD",
                Unit::Tokens => "tokens",
            },
        }));
        Ok(uses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iterations(used: f64) -> Use {
        Use {
            used,
            max: 20.0,
            unit: "iterations",
        }
    }

    #[test]
    fn the_most_constrained_budget_is_named_once_it_reaches_the_ratio() {
        assert_eq!(crossed(vec![iterations(14.0)], 0.75), None);
        let usd = Use {
            used: 0.4,
            max: 0.5,
            unit: "USD",
        };
        let hit = crossed(vec![iterations(15.0), usd], 0.75).unwrap();
        assert_eq!(hit.notice(), "[budget checkpoint] used 0.40 of 0.50 USD");
        assert_eq!(
            crossed(vec![iterations(15.0)], 0.75).unwrap().notice(),
            "[budget checkpoint] used 15 of 20 iterations"
        );
    }

    #[test]
    fn unlimited_budgets_never_count() {
        let unlimited = Use {
            used: 5.0,
            max: 0.0,
            unit: "tokens",
        };
        assert_eq!(crossed(vec![unlimited], 0.5), None);
    }
}
