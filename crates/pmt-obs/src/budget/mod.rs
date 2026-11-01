//! The budget engine (spec: observability, Hard budgets, Budget warnings
//! and overrides; agent-turn-loop, per-turn and per-session budgets and the
//! unpriced-model rule).
//!
//! Before every model call the caller asks [`Budgets::check`] with the
//! request's worst case (input tokens × input price + requested max output
//! tokens × output price). Spend comes from the usage ledger, so every
//! scope counts compression, auxiliary and delegated calls of its lineage.
//! Limits are passed in on every check (never cached), so a raised budget
//! applies to the very next call. A refusal is an error of kind
//! `budget_exceeded` (exit 3) plus the `budget.exceeded` event to record;
//! crossing `budgets.warn_ratio` yields one `budget.warning` per scope and
//! period.
//!
//! Model tools never change budgets: [`guard`] refuses agent writes to
//! `budgets.*`, including file writes to a profile's `config.toml`.

pub mod guard;
mod scope;

pub use scope::{Limit, Place, Scope, Unit, limits, set_usd};

use std::collections::HashSet;
use std::sync::Mutex;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use pmt_config::schema::BudgetsConfig;
use pmt_core::error::{Error, codes};
use pmt_store::{Store, UsageTotals};

use crate::trajectory::payload::Budget;
use crate::usage::Price;

/// The call about to be sent.
#[derive(Debug, Clone, PartialEq)]
pub struct Request<'a> {
    /// Where the call happens.
    pub place: &'a Place,
    /// Cron, unattended gateway session, background delegation or harness
    /// evolution: `budgets.unattended_root_usd` applies.
    pub unattended: bool,
    /// Model id (named in the unpriced refusal).
    pub model: &'a str,
    /// The model's rates, `None` when unpriced.
    pub price: Option<Price>,
    /// Input tokens of the request.
    pub input_tokens: u64,
    /// Requested maximum output tokens.
    pub max_output_tokens: u64,
}

impl Request<'_> {
    fn estimate(&self, unit: Unit) -> Option<f64> {
        match unit {
            #[allow(clippy::cast_precision_loss)]
            Unit::Tokens => Some(self.input_tokens.saturating_add(self.max_output_tokens) as f64),
            Unit::Usd => self
                .price
                .map(|price| price.worst_case(self.input_tokens, self.max_output_tokens)),
        }
    }
}

/// A call the budgets allow, with the warnings to emit (`budget.warning`
/// events and user-visible notices).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Allowed {
    /// Scopes whose spend first crossed the warning ratio.
    pub warnings: Vec<Budget>,
}

/// A refused call: the error ending the turn and the `budget.exceeded`
/// event to record.
#[derive(Debug)]
pub struct Refused {
    /// Kind `budget_exceeded` (`budget.exceeded` or
    /// `budget.unpriced_model`).
    pub error: Error,
    /// The `budget.exceeded` payload.
    pub event: Budget,
    /// Warnings due before the refusal.
    pub warnings: Vec<Budget>,
}

/// Budget state of one session: the profile time zone and which warnings
/// were already given. Limits are passed to every check.
#[derive(Debug)]
pub struct Budgets {
    zone: TimeZone,
    warned: Mutex<HashSet<(Scope, Unit, String)>>,
    unpriced_warned: Mutex<HashSet<String>>,
}

impl Budgets {
    /// Budgets evaluated in `zone` (days and months).
    #[must_use]
    pub fn new(zone: TimeZone) -> Self {
        Self {
            zone,
            warned: Mutex::new(HashSet::new()),
            unpriced_warned: Mutex::new(HashSet::new()),
        }
    }

    /// Checks a call against every applicable limit of `config`.
    ///
    /// # Errors
    ///
    /// [`Refused`] when the worst case would exceed a limit, or when a
    /// USD limit applies to an unpriced model and
    /// `budgets.allow_unpriced` is false; store errors become a refusal
    /// too (fail closed) with the store's error.
    pub fn check(
        &self,
        store: &Store,
        config: &BudgetsConfig,
        request: &Request<'_>,
        now: Timestamp,
    ) -> Result<Allowed, Box<Refused>> {
        let applicable = limits(
            config,
            request.place.turn_started_at.is_some(),
            request.unattended,
        );
        let mut spends = Vec::with_capacity(applicable.len());
        for limit in &applicable {
            let spent = self
                .spent(store, limit, request.place, now)
                .map_err(|error| refusal(error, limit, 0.0, None, Vec::new()))?;
            spends.push((*limit, spent));
        }
        let warnings = self.warnings_for(config, &spends);
        let usd_applies = applicable.iter().any(|l| l.unit == Unit::Usd);
        if usd_applies && request.price.is_none() {
            if !config.allow_unpriced {
                let limit = applicable.iter().find(|l| l.unit == Unit::Usd).copied();
                return Err(unpriced(request.model, limit, &spends, warnings));
            }
            self.warn_unpriced_once(request.model);
        }
        for (limit, (spent, _)) in &spends {
            let Some(estimate) = request.estimate(limit.unit) else {
                continue; // unpriced and allowed: only token limits apply
            };
            if spent + estimate > limit.limit {
                let error = exceeded(limit, *spent, estimate);
                return Err(refusal(error, limit, *spent, Some(estimate), warnings));
            }
        }
        Ok(Allowed { warnings })
    }

    /// Warnings due after a call was recorded (spend may have crossed the
    /// ratio).
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn after_call(
        &self,
        store: &Store,
        config: &BudgetsConfig,
        place: &Place,
        unattended: bool,
        now: Timestamp,
    ) -> Result<Vec<Budget>, Error> {
        let applicable = limits(config, place.turn_started_at.is_some(), unattended);
        let mut spends = Vec::with_capacity(applicable.len());
        for limit in applicable {
            spends.push((limit, self.spent(store, &limit, place, now)?));
        }
        Ok(self.warnings_for(config, &spends))
    }

    /// Every applicable limit with its current spend (for `/budget` and
    /// status displays).
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn status(
        &self,
        store: &Store,
        config: &BudgetsConfig,
        place: &Place,
        unattended: bool,
        now: Timestamp,
    ) -> Result<Vec<(Limit, f64)>, Error> {
        limits(config, place.turn_started_at.is_some(), unattended)
            .into_iter()
            .map(|limit| Ok((limit, self.spent(store, &limit, place, now)?.0)))
            .collect()
    }

    /// Spend of `limit`'s scope in `limit`'s unit, and the period key.
    fn spent(
        &self,
        store: &Store,
        limit: &Limit,
        place: &Place,
        now: Timestamp,
    ) -> Result<(f64, String), Error> {
        let (filter, period) = scope::window(limit.scope, place, now, &self.zone)?;
        let totals = store.usage_sum(&filter)?;
        Ok((amount(&totals, limit.unit), period))
    }

    fn warnings_for(
        &self,
        config: &BudgetsConfig,
        spends: &[(Limit, (f64, String))],
    ) -> Vec<Budget> {
        if config.warn_ratio <= 0.0 {
            return Vec::new();
        }
        let mut warned = self
            .warned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        spends
            .iter()
            .filter(|(limit, (spent, _))| *spent >= limit.limit * config.warn_ratio)
            .filter(|(limit, (_, period))| warned.insert((limit.scope, limit.unit, period.clone())))
            .map(|(limit, (spent, _))| event(limit, *spent, None))
            .collect()
    }

    fn warn_unpriced_once(&self, model: &str) {
        let mut warned = self
            .unpriced_warned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if warned.insert(model.to_owned()) {
            tracing::warn!(
                model,
                "model '{model}' has no price: USD budgets are not enforced for it (budgets.allow_unpriced = true)"
            );
        }
    }
}

fn amount(totals: &UsageTotals, unit: Unit) -> f64 {
    match unit {
        Unit::Usd => totals.cost_usd,
        #[allow(clippy::cast_precision_loss)]
        Unit::Tokens => totals.tokens.billed() as f64,
    }
}

fn event(limit: &Limit, spent: f64, estimate: Option<f64>) -> Budget {
    Budget {
        scope: limit.scope.as_str().to_owned(),
        unit: limit.unit.as_str().to_owned(),
        limit: limit.limit,
        spent,
        estimate,
    }
}

fn refusal(
    error: Error,
    limit: &Limit,
    spent: f64,
    estimate: Option<f64>,
    warnings: Vec<Budget>,
) -> Box<Refused> {
    Box::new(Refused {
        error,
        event: event(limit, spent, estimate),
        warnings,
    })
}

/// `<amount>` in the unit: `$0.98` or `90000 tokens`.
#[must_use]
pub fn format_amount(unit: Unit, value: f64) -> String {
    match unit {
        Unit::Usd => format!("${value:.2}"),
        Unit::Tokens => format!("{value:.0} tokens"),
    }
}

fn exceeded(limit: &Limit, spent: f64, estimate: f64) -> Error {
    let key = match limit.unit {
        Unit::Usd => limit.scope.usd_key().to_owned(),
        Unit::Tokens => format!("{}_tokens", limit.scope.as_str()),
    };
    Error::new(
        &codes::BUDGET_EXCEEDED,
        format!(
            "{} budget exceeded: spent {} of {}; the next call could cost up to {}",
            limit.scope.as_str(),
            format_amount(limit.unit, spent),
            format_amount(limit.unit, limit.limit),
            format_amount(limit.unit, estimate),
        ),
    )
    .with_hint(format!(
        "raise it with: prometheus config set budgets.{key} <value>"
    ))
}

fn unpriced(
    model: &str,
    limit: Option<Limit>,
    spends: &[(Limit, (f64, String))],
    warnings: Vec<Budget>,
) -> Box<Refused> {
    let error = Error::new(
        &codes::BUDGET_UNPRICED_MODEL,
        format!(
            "cost budget set but model '{model}' has no price; set models.{model}.price or budgets.allow_unpriced = true"
        ),
    );
    let limit = limit.unwrap_or(Limit {
        scope: Scope::Root,
        unit: Unit::Usd,
        limit: 0.0,
    });
    let spent = spends
        .iter()
        .find(|(l, _)| *l == limit)
        .map_or(0.0, |(_, (spent, _))| *spent);
    refusal(error, &limit, spent, None, warnings)
}

/// The user-visible notice of a warning:
/// `budget warning: root spend $0.81 of $1.00 (81%)`.
#[must_use]
pub fn notice(warning: &Budget) -> String {
    let unit = if warning.unit == "tokens" {
        Unit::Tokens
    } else {
        Unit::Usd
    };
    let percent = if warning.limit > 0.0 {
        warning.spent / warning.limit * 100.0
    } else {
        100.0
    };
    format!(
        "budget warning: {} spend {} of {} ({percent:.0}%)",
        warning.scope,
        format_amount(unit, warning.spent),
        format_amount(unit, warning.limit),
    )
}
