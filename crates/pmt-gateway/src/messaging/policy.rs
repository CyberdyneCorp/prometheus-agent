//! A chat policy as the session it shapes (spec: messaging-gateway,
//! Per-chat policies): the model, toolsets, skills, system prompt addition
//! and per-turn cost cap become the session's own setup, so a policy
//! frozen with a session keeps shaping it after config edits and daemon
//! restarts. Busy and rate-limit overrides stay with the gateway.

use pmt_config::schema::ChatPolicy;
use pmt_rpc::contracts::SessionSetup;

fn quoted(text: &str) -> String {
    serde_json::to_string(text).expect("a string serializes")
}

fn list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|i| quoted(i)).collect();
    format!("[{}]", quoted.join(", "))
}

/// The session setup of `policy` for a session on `platform`.
#[must_use]
pub fn session_setup(policy: &ChatPolicy, platform: &str) -> SessionSetup {
    let mut setup = SessionSetup {
        model: policy.model.clone(),
        skills: policy.skills.clone().unwrap_or_default(),
        ..SessionSetup::default()
    };
    if let Some(toolsets) = &policy.toolsets {
        // The platform's surface list: the session starts from it instead
        // of the platform bundle (spec: tool-system, Per-surface default
        // toolsets).
        setup.set.push(format!(
            "toolsets.surfaces.gateway.{}={}",
            platform.to_lowercase(),
            list(toolsets)
        ));
    }
    if let Some(text) = &policy.system_prompt_append {
        setup.set.push(format!(
            "prompt.platform_hints.{platform}.append={}",
            quoted(text)
        ));
    }
    if let Some(usd) = policy.max_cost_usd_per_turn {
        setup.set.push(format!("budgets.turn_usd={usd}"));
    }
    setup
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_session_shaping_key_becomes_setup() {
        let policy = ChatPolicy {
            platform: "discord".into(),
            chat_id: Some("555".into()),
            model: Some("mock/small".into()),
            toolsets: Some(vec!["web".into()]),
            skills: Some(vec!["triage".into()]),
            system_prompt_append: Some("Answer in \"French\".".into()),
            max_cost_usd_per_turn: Some(0.5),
            ..ChatPolicy::default()
        };
        let setup = session_setup(&policy, "discord");
        assert_eq!(setup.model.as_deref(), Some("mock/small"));
        assert_eq!(setup.skills, vec!["triage"]);
        let overrides = pmt_config::Overrides::parse(&setup.set).unwrap();
        assert!(!overrides.is_empty());
        assert!(setup.set.contains(&"budgets.turn_usd=0.5".to_owned()));
        assert!(
            setup
                .set
                .contains(&"toolsets.surfaces.gateway.discord=[\"web\"]".to_owned())
        );
    }
}
