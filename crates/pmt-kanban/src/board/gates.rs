//! The upgrade completion gates (spec: kanban, PR completion contracts;
//! Goal-mode cards): a repository contract binds the first matching
//! `metadata.published_pr` for good and completes only on green required
//! checks of that PR; a goal-mode task judged unachievable refuses every
//! handoff until a person lifts it.

use pmt_core::error::Error;
use rusqlite::params;
use serde_json::{Value, json};

use super::finish::Refusal;
use super::{Board, event};
use crate::contract::{Acceptance, Classification, Contract, PrRef};
use crate::db::sql;
use crate::model::{Actor, Task};

/// Events after which an earlier goal verdict no longer holds.
const GOAL_RESETS: [&str; 3] = ["unblocked", "restored", "moved"];

fn refusal(message: &str) -> Refusal {
    Refusal::gate(format!("completion blocked: {message}"))
}

impl Board {
    /// The PR a completion of `task` names, bound to the task the first
    /// time; `Err` is why it cannot be used.
    fn published(
        &mut self,
        actor: &Actor,
        task: &Task,
        contract: &Contract,
        metadata: Option<&Value>,
        now: i64,
    ) -> Result<Result<PrRef, Refusal>, Error> {
        let given = metadata
            .and_then(|m| m.get("published_pr"))
            .and_then(Value::as_str);
        let Some(url) = given.map(str::to_owned).or_else(|| task.pr_url.clone()) else {
            return Ok(Err(refusal(&format!(
                "completion contract {contract} requires metadata.published_pr naming its pull request"
            ))));
        };
        let Some(pr) = PrRef::parse(&url) else {
            return Ok(Err(refusal(&format!(
                "metadata.published_pr '{url}' is not a GitHub pull-request URL"
            ))));
        };
        if !contract.admits(&pr) {
            return Ok(Err(refusal(&format!(
                "{url} is not a pull request of completion contract {contract}"
            ))));
        }
        if let Some(bound) = &task.pr_url {
            if PrRef::parse(bound).as_ref() != Some(&pr) {
                return Ok(Err(refusal(&format!(
                    "completion contract already bound to {bound}"
                ))));
            }
            return Ok(Ok(pr));
        }
        let label = actor.label();
        let url = pr.url();
        self.write(|tx| {
            tx.execute(
                "UPDATE tasks SET pr_url = ?2 WHERE id = ?1 AND pr_url IS NULL",
                params![task.id, url],
            )
            .map_err(sql)?;
            event(
                tx,
                Some(&task.id),
                actor.run.as_deref(),
                &label,
                "pr_bound",
                &json!({"pr": url}),
                now,
            )?;
            Ok(())
        })?;
        Ok(Ok(pr))
    }

    /// Binds `metadata.published_pr` of a handoff to review (no checks:
    /// they gate the approval).
    pub(super) fn bind_published(
        &mut self,
        actor: &Actor,
        task: &Task,
        metadata: Option<&Value>,
        now: i64,
    ) -> Result<(), Error> {
        let contract = Contract::of(task.completion_contract.as_deref())?;
        let named = metadata.is_some_and(|m| m.get("published_pr").is_some());
        if contract == Contract::LocalOnly || !named {
            return Ok(());
        }
        match self.published(actor, task, &contract, metadata, now)? {
            Ok(_) => Ok(()),
            Err(refusal) => Err(self.refuse(actor, &task.id, &refusal, now)),
        }
    }

    /// The contract gate of a completion reaching `done`: `None` when the
    /// task may complete.
    pub(super) fn contract_gate(
        &mut self,
        actor: &Actor,
        task: &Task,
        metadata: Option<&Value>,
        now: i64,
    ) -> Result<Option<Refusal>, Error> {
        let contract = Contract::of(task.completion_contract.as_deref())?;
        if contract == Contract::LocalOnly {
            return Ok(None);
        }
        let pr = match self.published(actor, task, &contract, metadata, now)? {
            Ok(pr) => pr,
            Err(refusal) => return Ok(Some(refusal)),
        };
        let profile = task.assignee.clone().unwrap_or_else(|| actor.name.clone());
        let acceptance = match self.pr_checks() {
            Some(checks) => checks.evaluate(&profile, &pr),
            None => Acceptance::refused(
                &pr,
                Classification::Infra,
                "this surface cannot read GitHub checks",
                "complete the task from the CLI or its worker",
            ),
        };
        if acceptance.accepted() {
            let payload = json!({"pr": acceptance.pr, "head_sha": acceptance.head_sha, "required": acceptance.required});
            self.record(
                Some(&task.id),
                actor.run.as_deref(),
                actor,
                "pr_accepted",
                &payload,
                now,
            )?;
            return Ok(None);
        }
        Ok(Some(Refusal {
            kind: "pr_acceptance",
            message: format!("completion blocked: {}", acceptance.reason),
            payload: Some(serde_json::to_value(&acceptance).unwrap_or_default()),
        }))
    }

    /// Refuses the handoff of a goal-mode task whose latest verdict (since
    /// a person last lifted it) is `unachievable`.
    pub(super) fn goal_gate(&self, task: &Task) -> Result<Option<Refusal>, Error> {
        if !task.goal_mode {
            return Ok(None);
        }
        let events = self.events(Some(&task.id))?;
        let latest = events
            .iter()
            .rev()
            .take_while(|e| !GOAL_RESETS.contains(&e.kind.as_str()))
            .find(|e| e.kind == "goal_verdict");
        Ok(latest
            .filter(|e| e.payload["verdict"] == "unachievable")
            .map(|e| {
                let reason = e.payload["reason"].as_str().unwrap_or("no reason given");
                refusal(&format!("goal judged unachievable: {reason}"))
            }))
    }

    /// Records one judged turn of a goal-mode worker.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn goal_verdict(
        &mut self,
        actor: &Actor,
        id: &str,
        (turn, max): (u32, u32),
        verdict: &str,
        reason: &str,
        now: i64,
    ) -> Result<(), Error> {
        let payload = json!({"turn": turn, "max_turns": max, "verdict": verdict, "reason": reason});
        self.record(
            Some(id),
            actor.run.as_deref(),
            actor,
            "goal_verdict",
            &payload,
            now,
        )
        .map(|_| ())
    }
}
