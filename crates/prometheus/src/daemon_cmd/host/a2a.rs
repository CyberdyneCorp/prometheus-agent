//! What the A2A server and the peer-facing API endpoints need from the
//! daemon (spec: agent-interop): push notifications under the profile's
//! egress policy, the Agent Card's skills (the toolsets inbound A2A
//! sessions get) and the served profiles as bots (`GET /api/agents`).

use std::collections::BTreeSet;
use std::sync::Arc;

use pmt_api::Push;
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_tools::selection::SessionSurface;
use serde_json::{Value, json};

use super::{DaemonHost, Profile};

/// How long one push may take.
const PUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// A fetcher bound by the profile's egress policy (private, loopback and
/// link-local destinations refused unless allowlisted or granted).
///
/// # Errors
///
/// A bad egress configuration; the client cannot be built.
pub(super) fn fetcher(scope: &ProfileScope) -> Result<pmt_net::fetch::Fetcher, Error> {
    crate::egress::fetcher(scope, "a2a", None)
}

/// Sends one push notification; the HTTP status.
pub(super) async fn push(host: &DaemonHost, push: Push<'_>) -> Result<u16, Error> {
    let profile = host.profile(push.profile)?;
    let fetcher = fetcher(&profile.scope())?;
    let body = push.body.to_vec();
    let headers = push.headers.to_vec();
    let response = fetcher
        .send(reqwest::Method::POST, push.url, move |mut request| {
            for (name, value) in &headers {
                request = request.header(name, value);
            }
            request.body(body).timeout(PUSH_TIMEOUT)
        })
        .await?;
    Ok(response.status().as_u16())
}

/// The toolsets of inbound A2A sessions, narrowed to
/// `a2a.server.advertised_toolsets`: a toolset they cannot use is never
/// listed.
pub(super) fn skills(scope: &ProfileScope) -> Result<Value, Error> {
    let config = scope.config();
    let (registry, extensions) = crate::tools_cmd::profile_registry(scope)?;
    let selection = crate::chat::session::tools::selection(
        &registry,
        scope,
        false,
        &extensions,
        &SessionSurface::A2a,
    )?;
    let advertised: Option<BTreeSet<&str>> = config
        .a2a
        .server
        .advertised_toolsets
        .as_ref()
        .map(|list| list.iter().map(String::as_str).collect());
    let rows = crate::tools_cmd::toolset_rows(&registry, &BTreeSet::new())?;
    Ok(rows
        .iter()
        .filter(|row| {
            registry.members(&row.name).is_ok_and(|members| {
                !members.is_empty() && members.iter().all(|m| selection.contains(m))
            })
        })
        .filter(|row| {
            advertised
                .as_ref()
                .is_none_or(|a| a.contains(row.name.as_str()))
        })
        .map(|row| json!({"name": row.name, "description": row.description}))
        .collect())
}

/// Every profile the daemon serves, as a bot row of `GET /api/agents`.
pub(super) fn agents(host: &DaemonHost) -> Value {
    let rows: Vec<Value> = host
        .all_profiles()
        .iter()
        .map(|(_, profile)| agent_row(profile))
        .collect();
    Value::Array(rows)
}

fn agent_row(profile: &Arc<Profile>) -> Value {
    let scope = profile.scope();
    let config = scope.config();
    let name = scope.name().to_string();
    json!({
        "name": name,
        "bot": true,
        "title": Value::Null,
        "description": config.a2a.server.description,
        "avatar": Value::Null,
        "model": config.model.default,
        "provider": config.model.provider,
        "accepts_bot_messages": true,
    })
}

/// The prompt-injection findings of `text`.
pub(super) fn scan(text: &str) -> Vec<String> {
    pmt_agent::prompt::scan::findings(text)
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// Refuses `url` when the profile's egress policy would.
pub(super) async fn push_check(host: &DaemonHost, profile: &str, url: &str) -> Result<(), Error> {
    let profile = host.profile(profile)?;
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(Error::new(
            &codes::NET_INVALID_URL,
            format!("push URL '{url}' is not http(s)"),
        ));
    }
    fetcher(&profile.scope())?.check(url).await
}
