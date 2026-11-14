//! The live subagents frame (spec: desktop-app, Live subagents frame):
//! a session's delegated children as the frame lists them, and its Steer
//! and Stop controls, which go through the session's own delegation
//! (the `delegate_task` control actions, scoped to its spawn tree).

use pmt_core::cancel::CancelToken;
use pmt_core::error::{Error, codes};
use pmt_delegation::tree::Registry;
use pmt_rpc::contracts::{SubagentList, SubagentSummary};
use serde_json::{Value, json};

use super::{DaemonHost, Profile, session_id};

impl DaemonHost {
    async fn delegator(
        profile: &Profile,
        session: &str,
    ) -> Result<Option<std::sync::Arc<pmt_delegation::host::Delegator>>, Error> {
        let id = session_id(session)?;
        Ok(profile
            .loaded
            .lock()
            .await
            .get(&id)
            .and_then(|s| s.live.delegation.clone()))
    }

    pub(super) async fn subagents_of(
        &self,
        profile: &Profile,
        session: &str,
    ) -> Result<SubagentList, Error> {
        let Some(delegator) = Self::delegator(profile, session).await? else {
            return Ok(SubagentList::default());
        };
        let subagents = Registry::process()
            .tree(&delegator.conversation())
            .iter()
            .map(|entry| summary(&entry.view()))
            .collect();
        Ok(SubagentList { subagents })
    }

    pub(super) async fn control_subagent(
        &self,
        profile: &Profile,
        session: &str,
        subagent_id: &str,
        steer: Option<&str>,
    ) -> Result<String, Error> {
        let delegator = Self::delegator(profile, session).await?.ok_or_else(|| {
            Error::new(
                &codes::DELEGATION_INVALID_TASK,
                "this session has no running subagents",
            )
        })?;
        let arguments = match steer {
            Some(text) => json!({"action": "steer", "subagent_id": subagent_id, "message": text}),
            None => json!({"action": "stop", "subagent_id": subagent_id}),
        };
        let result = delegator.call(&arguments, &CancelToken::new()).await;
        let text = result.text_content();
        serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("status").and_then(Value::as_str).map(str::to_owned))
            .ok_or_else(|| Error::new(&codes::DELEGATION_INVALID_TASK, text))
    }
}

/// A child as the frame shows it, from its control view.
fn summary(view: &Value) -> SubagentSummary {
    let text = |key: &str| view.get(key).and_then(Value::as_str).map(str::to_owned);
    SubagentSummary {
        subagent_id: text("subagent_id").unwrap_or_default(),
        session_id: text("parent_session_id").unwrap_or_default(),
        status: text("status").unwrap_or_default(),
        title: text("goal"),
        elapsed_ms: view.get("elapsed_ms").and_then(Value::as_u64),
        activity: text("activity"),
    }
}
