//! The budget engine at each model call (spec: agent-turn-loop, Per-turn
//! and per-session token and cost budgets; Unpriced models fail closed
//! under a cost budget; observability, Hard budgets).
//!
//! Before every call the request's worst case is checked against every
//! applicable `budgets.*` limit (spend comes from the usage ledger, so
//! compression, auxiliary and delegated calls count); a refusal ends the
//! turn with `token_budget_exhausted` or `cost_budget_exhausted` and the
//! summary handoff, except an unpriced model under a USD budget, which
//! ends the turn without calling the model.

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_obs::budget::{self, Place};
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::Budget;
use pmt_providers::request::Request;

use super::exit::ExitReason;
use super::model::RouteInfo;
use super::run::{Ending, Turn};

impl Turn<'_> {
    fn place(&self) -> Result<Place, Error> {
        let setup = &self.agent.setup;
        Place::of(&setup.store, setup.session, Some(self.started_at))
    }

    /// `Some(ending)` when the budgets refuse the call.
    pub(super) async fn check_budget(
        &mut self,
        request: &Request,
        route: &RouteInfo,
    ) -> Result<Option<Ending>, Error> {
        let setup = &self.agent.setup;
        let place = self.place()?;
        let call = budget::Request {
            place: &place,
            unattended: setup.unattended,
            model: &route.model,
            price: route.price,
            input_tokens: request.estimated_tokens(),
            max_output_tokens: request.max_output_tokens.unwrap_or(0),
        };
        let checked =
            self.agent
                .budgets
                .check(&setup.store, &self.config.budgets, &call, Timestamp::now());
        match checked {
            Ok(allowed) => {
                self.warn(allowed.warnings).await;
                Ok(None)
            }
            Err(refused) => {
                let refused = *refused;
                self.warn(refused.warnings).await;
                let reason = if refused.event.unit == "tokens" {
                    ExitReason::TokenBudgetExhausted
                } else {
                    ExitReason::CostBudgetExhausted
                };
                self.emit(Event::BudgetExceeded(refused.event)).await;
                self.notices.push(refused.error.message().to_owned());
                let unpriced = refused.error.code().code == "budget.unpriced_model";
                let mut end = Ending::failed(reason, refused.error);
                end.handoff = !unpriced;
                Ok(Some(end))
            }
        }
    }

    /// Warnings due after a call was recorded.
    pub(super) async fn after_call(&mut self) -> Result<(), Error> {
        let setup = &self.agent.setup;
        let place = self.place()?;
        let warnings = self.agent.budgets.after_call(
            &setup.store,
            &self.config.budgets,
            &place,
            setup.unattended,
            Timestamp::now(),
        )?;
        self.warn(warnings).await;
        Ok(())
    }

    async fn warn(&mut self, warnings: Vec<Budget>) {
        for warning in warnings {
            self.notices.push(budget::notice(&warning));
            self.emit(Event::BudgetWarning(warning)).await;
        }
    }
}
