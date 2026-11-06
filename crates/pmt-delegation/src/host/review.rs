//! A labelled one-task background dispatch: the delegation rail `/review`
//! runs on (spec: subagent-delegation, Review command). The child is a
//! normal child of the session (its grant, limits, approvals and
//! transcripts), and its completion re-enters the session as a
//! `delegation_completed` input headed by the label.

use std::sync::Arc;

use pmt_core::cancel::CancelToken;
use pmt_core::error::codes;
use serde_json::json;

use super::spawn::{Dispatched, Mode};
use super::units::Unit;
use super::{Delegator, Dispatch};
use crate::Refusal;
use crate::contract;

/// What a labelled dispatch runs.
#[derive(Debug, Clone)]
pub struct Labelled {
    /// The child's goal.
    pub goal: String,
    /// The context it needs.
    pub context: String,
    /// Its model (`provider/model`); `None` follows the usual routing.
    pub model: Option<String>,
    /// What heads its completion (`Review: <focus>`).
    pub label: String,
}

impl Delegator {
    /// Dispatches `request` in the background; its delegation id.
    ///
    /// # Errors
    ///
    /// Every refusal a spawn of this session would meet, and
    /// `delegation.invalid_task` when the session cannot receive a later
    /// completion.
    pub async fn dispatch_labelled(self: &Arc<Self>, request: Labelled) -> Result<String, Refusal> {
        let Dispatch::Background(sink) = &self.ctx.dispatch else {
            return Err(Refusal::new(
                &codes::DELEGATION_INVALID_TASK,
                "This session cannot receive later completions, so it cannot run a background review.",
            ));
        };
        let mut task = json!({"goal": request.goal, "context": request.context});
        if let Some(model) = &request.model {
            task["model"] = model.clone().into();
        }
        let call = contract::parse(&json!({"tasks": [task]}), 1, &self.router_params())?;
        let (planned, admissions) = self.admit(&call).await?;
        let id = crate::ids::delegation_id();
        let units = [Unit::whole(&id, 1)];
        let mut children = self.register(
            &units,
            &call,
            planned,
            admissions,
            Mode::Background,
            &CancelToken::new(),
        )?;
        let live = self.transcripts(&id, &mut children);
        let dispatched = Dispatched {
            units: &units,
            children,
            live,
            independent: false,
        };
        self.dispatch_background(&id, dispatched, sink, Some(&request.label));
        Ok(id)
    }
}
