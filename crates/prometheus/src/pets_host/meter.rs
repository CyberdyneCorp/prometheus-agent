//! Billing of `/hatch`'s paid image calls (spec: observability, Per-call
//! usage ledger, Hard budgets): every draft and animation row is one
//! `tool.image_generate` row of the usage ledger (`purpose_detail =
//! pets.hatch`), and each is checked against the profile's budgets before
//! it is sent, like any other metered call.

use std::sync::{Arc, Mutex, PoisonError};

use pmt_config::ProfileScope;
use pmt_config::schema::BudgetsConfig;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_media::image::Plan;
use pmt_obs::budget::{Budgets, Place, Request};
use pmt_obs::usage::{Call, Price, UsageRecorder};
use pmt_store::{NewSession, Source, Store};

/// The ledger purpose of a generated image.
pub const PURPOSE: &str = "tool.image_generate";
/// Its detail for pet generation.
pub const DETAIL: &str = "pets.hatch";

/// The session a hatch's calls are billed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Billing {
    /// The session the user is in (`/hatch` in a chat).
    Session(SessionId),
    /// A hidden session made on the first paid call, from `Source`
    /// (`prometheus pets hatch`, the desktop generate flow outside a chat).
    Hidden(Source),
}

/// Whether `plan` bills (a local backend records nothing, as the image
/// tool's own local calls do not).
fn metered(plan: &Plan) -> bool {
    let source = plan.access.billing_source.as_str();
    !source.is_empty() && source != "local"
}

/// The ledger, recorder and budgets of one hatch.
pub struct Meter {
    store: Arc<Store>,
    recorder: UsageRecorder,
    budgets: Budgets,
    limits: BudgetsConfig,
    billing: Mutex<Billing>,
}

impl Meter {
    /// The profile's meter for a hatch billed per `billing`.
    ///
    /// # Errors
    ///
    /// Store errors; an invalid `cron.timezone`.
    pub fn open(scope: &ProfileScope, billing: Billing) -> Result<Self, Error> {
        let store = Arc::new(Store::open(scope)?);
        if let Billing::Session(session) = billing {
            store.session(session)?;
        }
        let zone = pmt_obs::time::profile_zone(&scope.config().cron)?;
        Ok(Self {
            recorder: crate::chat::session::model::profile_recorder(scope, Arc::clone(&store)),
            store,
            budgets: Budgets::new(zone),
            limits: scope.config().budgets.clone(),
            billing: Mutex::new(billing),
        })
    }

    /// The billed session, creating the hidden one on first use.
    fn session(&self) -> Result<SessionId, String> {
        let mut billing = self.billing.lock().unwrap_or_else(PoisonError::into_inner);
        match *billing {
            Billing::Session(session) => Ok(session),
            Billing::Hidden(source) => {
                let new = NewSession {
                    hidden: true,
                    title: Some("Pet generation".to_owned()),
                    ..NewSession::new(source)
                };
                let session = self
                    .store
                    .create_session(&new)
                    .map_err(|e| e.message().to_owned())?
                    .id;
                *billing = Billing::Session(session);
                Ok(session)
            }
        }
    }

    /// Checks one image of `plan` costing `usd` (`None`: unpriced, which a
    /// USD budget refuses unless `budgets.allow_unpriced`) against every
    /// budget; returns the session to bill.
    ///
    /// # Errors
    ///
    /// The budget refusal, as the text the hatch shows.
    pub fn allow(&self, plan: &Plan, usd: Option<f64>) -> Result<SessionId, String> {
        let session = self.session()?;
        if !metered(plan) {
            return Ok(session);
        }
        let place = Place::of(&self.store, session, None).map_err(|e| e.message().to_owned())?;
        // One image is priced as one "input token" at the per-image rate,
        // so the engine's worst case is exactly the image's estimate.
        let request = Request {
            place: &place,
            unattended: false,
            model: &plan.model.id,
            price: usd.map(|usd| Price {
                input: usd * 1_000_000.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
                reasoning: None,
            }),
            input_tokens: 1,
            max_output_tokens: 0,
        };
        let allowed = self
            .budgets
            .check(&self.store, &self.limits, &request, jiff::Timestamp::now())
            .map_err(|refused| refused.error.message().to_owned())?;
        for warning in allowed.warnings {
            tracing::warn!(?warning, "pet generation is near a budget");
        }
        Ok(session)
    }

    /// Records one generated image of `plan` in `session`'s ledger.
    pub fn record(&self, session: SessionId, plan: &Plan, usd: Option<f64>) {
        if !metered(plan) {
            return;
        }
        let call = Call {
            purpose_detail: Some(DETAIL.to_owned()),
            billing_source: plan.access.billing_source.clone(),
            ..Call::new(session, PURPOSE, &plan.provider, &plan.model.id)
        };
        if let Err(err) = self.recorder.record_estimated(call, usd) {
            tracing::warn!(error = %err, "pet generation usage not recorded");
        }
    }
}
