//! `/review [instructions]` (spec: subagent-delegation, Review command):
//! a reviewer child on the delegation rail, in the background. Its
//! evidence is a snapshot of the session's last 10 user and assistant
//! messages read from storage (tool output and system messages left
//! out), with a briefing naming the skills the session loaded and the
//! instructions as its focus; it has a normal child's tools under the
//! session's grant. Its model is `[delegation.review]` (default: the
//! session's route). Its findings re-enter the session as a
//! `delegation_completed` input labelled `Review: <focus>` (or `Review
//! recent work`).

use pmt_core::error::{Error, codes};
use pmt_delegation::host::Labelled;
use pmt_store::{Role, Visibility};
use serde_json::Value;

use super::Ctx;

/// What a started review answers.
const STARTED: &str = "Review started. Results will return here.";

/// User and assistant messages the reviewer sees.
const EVIDENCE: usize = 10;

pub(super) async fn review(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let Some(delegator) = &ctx.live.delegation else {
        return Err(Error::new(
            &codes::DELEGATION_INVALID_TASK,
            "this session cannot delegate, so it cannot run a review",
        ));
    };
    let focus = args.trim();
    let label = if focus.is_empty() {
        "Review recent work".to_owned()
    } else {
        format!("Review: {focus}")
    };
    let messages = ctx
        .factory
        .store
        .messages(ctx.live.id, Visibility::Active)?;
    let request = Labelled {
        goal: goal(focus),
        context: context(&messages),
        model: model(ctx),
        label,
    };
    delegator
        .dispatch_labelled(request)
        .await
        .map_err(|refusal| refusal.error())?;
    Ok(STARTED.to_owned())
}

fn goal(focus: &str) -> String {
    let mut goal = "Review the recent work of the session described in your context: check \
                    what was changed or claimed, and report concrete findings (defects, \
                    risks, missing tests, unverified claims) most important first, with file \
                    and line references where you can."
        .to_owned();
    if !focus.is_empty() {
        goal.push_str("\nFocus: ");
        goal.push_str(focus);
    }
    goal
}

/// The evidence snapshot and the skills briefing.
fn context(messages: &[pmt_store::messages::Message]) -> String {
    let said: Vec<String> = messages
        .iter()
        .filter(|m| matches!(m.role, Role::User | Role::Assistant))
        .filter_map(|m| {
            let text = m.content.as_deref()?.trim();
            (!text.is_empty()).then(|| {
                let who = if m.role == Role::User {
                    "User"
                } else {
                    "Assistant"
                };
                format!("{who}: {text}")
            })
        })
        .collect();
    let recent = &said[said.len().saturating_sub(EVIDENCE)..];
    let skills = loaded_skills(messages);
    let skills = if skills.is_empty() {
        "none".to_owned()
    } else {
        skills.join(", ")
    };
    format!(
        "The session's last {} user and assistant messages:\n\n{}\n\nSkills the session had \
         loaded: {skills}.",
        recent.len(),
        recent.join("\n\n")
    )
}

/// The skills the session loaded with `skill_view`, in order.
fn loaded_skills(messages: &[pmt_store::messages::Message]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let calls = messages
        .iter()
        .filter_map(|m| m.tool_calls.as_ref()?.as_array().cloned())
        .flatten();
    for call in calls {
        if call.get("name").and_then(Value::as_str) != Some("skill_view") {
            continue;
        }
        let arguments = call
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or("{}");
        let name = serde_json::from_str::<Value>(arguments)
            .ok()
            .and_then(|a| a.get("name").and_then(Value::as_str).map(str::to_owned));
        if let Some(name) = name.filter(|n| !names.contains(n)) {
            names.push(name);
        }
    }
    names
}

/// `[delegation.review]`'s model, else the session's route.
fn model(ctx: &Ctx<'_>) -> Option<String> {
    let review = &ctx.scope.config().delegation.review;
    let session = ctx.live.model.target();
    match (&review.provider, &review.model) {
        (Some(provider), Some(model)) => Some(format!("{provider}/{model}")),
        (None, Some(model)) => Some(model.clone()),
        (Some(provider), None) => session.model.map(|model| format!("{provider}/{model}")),
        (None, None) => session
            .model
            .map(|model| format!("{}/{model}", session.provider)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_focus_ends_the_goal() {
        assert!(goal("security").ends_with("\nFocus: security"));
        assert!(!goal("").contains("Focus:"));
    }
}
