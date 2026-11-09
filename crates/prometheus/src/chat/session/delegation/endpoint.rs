//! A child's route when the launching profile configures one for every
//! child (spec: subagent-delegation, Child request overrides and direct
//! endpoints): `delegation.base_url` (with `delegation.api_key_secret`
//! and `delegation.wire_format`) is a direct endpoint children call
//! instead of a provider route, and `delegation.request_overrides` are
//! set on every child model call whatever its route.

use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_config::schema::{DelegationConfig, WireFormat};
use pmt_delegation::routing::{ModelChoice, Source};
use pmt_providers::resolve::Target;
use serde_json::{Map, Value};

/// The provider a direct endpoint resolves through.
const DIRECT_PROVIDER: &str = "custom";

/// The direct endpoint target of a child choosing `model` (always
/// pinned: no inherited fallback chain), unless the task or its agent
/// pinned a route of its own; `parent` is the parent's route.
#[must_use]
pub(super) fn direct(
    delegation: &DelegationConfig,
    model: &ModelChoice,
    parent: &Target,
) -> Option<Target> {
    let base_url = delegation.base_url.clone()?;
    let model = match model.source {
        Source::Task | Source::Agent => return None,
        // `delegation.model` names the model at the endpoint.
        Source::Config => model.model.clone(),
        Source::Parent => parent.model.clone(),
    };
    Some(Target {
        provider: DIRECT_PROVIDER.to_owned(),
        model,
        base_url: Some(base_url),
        credential: delegation.api_key_secret.as_ref().map(|s| s.0.clone()),
    })
}

/// `scope` as children reaching the direct endpoint resolve it: the
/// endpoint's provider speaks the configured wire format (the resolver
/// honours it over its URL rules) and `api_key_secret` may be sent to the
/// endpoint's host. `None` without a direct endpoint.
#[must_use]
pub(super) fn scope_for(
    scope: &ProfileScope,
    delegation: &DelegationConfig,
) -> Option<ProfileScope> {
    let host = pmt_config::provider_urls::host_of(delegation.base_url.as_deref()?)?;
    let wire = delegation.wire_format;
    Some(scope.patched(move |config| {
        let provider = config
            .providers
            .entry(DIRECT_PROVIDER.to_owned())
            .or_default();
        if wire.is_some() {
            provider.wire_format = wire;
        }
        if !provider.key_hosts.contains(&host) {
            provider.key_hosts.push(host);
        }
    }))
}

/// What distinguishes factories of a direct endpoint configuration.
#[must_use]
pub(super) fn signature(delegation: &DelegationConfig) -> Option<String> {
    let url = delegation.base_url.as_deref()?;
    let wire = delegation.wire_format.map_or("", WireFormat::as_str);
    Some(format!("{url} {wire}"))
}

/// `delegation.request_overrides` as set on a request body; `None` when
/// there are none.
#[must_use]
pub(super) fn overrides(delegation: &DelegationConfig) -> Option<Arc<Map<String, Value>>> {
    if delegation.request_overrides.is_empty() {
        return None;
    }
    let map: Map<String, Value> = delegation
        .request_overrides
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    Some(Arc::new(map))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_task_pinned_model_keeps_its_own_route() {
        let delegation = DelegationConfig {
            base_url: Some("http://127.0.0.1:9/v1".to_owned()),
            ..DelegationConfig::default()
        };
        let parent = Target {
            provider: "openrouter".to_owned(),
            model: Some("parent-model".to_owned()),
            ..Target::default()
        };
        let task = ModelChoice {
            model: Some("openai/gpt-x".to_owned()),
            source: Source::Task,
        };
        assert!(direct(&delegation, &task, &parent).is_none());
        let inherited = ModelChoice {
            model: None,
            source: Source::Parent,
        };
        let route = direct(&delegation, &inherited, &parent).unwrap();
        assert_eq!(route.provider, "custom");
        assert_eq!(route.model.as_deref(), Some("parent-model"));
        assert!(direct(&DelegationConfig::default(), &inherited, &parent).is_none());
    }
}
