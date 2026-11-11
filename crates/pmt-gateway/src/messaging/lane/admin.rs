//! Admin-tier chat commands and the conversation's frozen policy (spec:
//! messaging-gateway, Global emergency pause; Home channel and /sethome;
//! Per-chat policies):
//!
//! - `/pause [reason|off|status]` writes or removes the home's pause
//!   sentinel (turns already running finish; new ones are refused);
//! - `/sethome` makes the current chat (and thread) the platform's home
//!   channel through the validated config writer;
//! - `/policy [show|apply --now]` shows the chat's policy, or replaces the
//!   session so an edited policy applies at once (`reset_reason =
//!   "policy"`).
//!
//! A policy is resolved when the conversation's session is created and
//! frozen with it in the store, so a later edit reaches new sessions only.

use pmt_config::schema::{ChatPolicy, EffectiveBusy, PolicyFacts, resolve_policy};
use pmt_store::gateway::routes::ResetReason;

use super::Lane;
use crate::messaging::key::Routing;

impl Lane {
    /// The policy matching `routing` in the profile's current config.
    pub(super) fn current_policy(&self, routing: &Routing) -> Option<ChatPolicy> {
        let scope = self.config()?;
        let facts = PolicyFacts {
            platform: &routing.platform,
            chat_id: &routing.chat_id,
            chat_type: routing.chat_type.as_str(),
            user_id: &routing.sender_id,
            thread_id: routing.thread_id.as_deref(),
        };
        resolve_policy(&scope.config().gateway.chat_policies, &facts).map(|(_, p)| p.clone())
    }

    /// The policy a new session of `routing` starts with: the matching
    /// chat policy plus what the adapter asks of sessions in that chat or
    /// thread (a topic's skill, a webhook route's skills and toolsets).
    pub(super) fn session_policy(&self, routing: &Routing) -> Option<ChatPolicy> {
        let hints = self
            .shared
            .adapters
            .adapter(&self.profile, &routing.platform)
            .map(|a| a.session_hints(&routing.chat_id, routing.thread_id.as_deref()))
            .unwrap_or_default();
        let policy = with_hints(self.current_policy(routing), hints, routing);
        with_model(policy, self.chat_model(routing), routing)
    }

    /// Freezes `policy` for `session` (stored with the session).
    pub(super) fn freeze_policy(&mut self, session: &str, policy: Option<ChatPolicy>) {
        if let (Some(store), Ok(id)) = (self.shared.store(&self.profile), session.parse()) {
            let text = policy
                .as_ref()
                .map(|p| serde_json::to_string(p).expect("a policy serializes"));
            if let Err(err) = store.set_session_policy(id, text.as_deref()) {
                tracing::warn!("chat policy not stored with the session: {err}");
            }
        }
        self.policy = policy;
    }

    /// Reads the policy frozen for `session`.
    pub(super) fn load_policy(&mut self, session: &str) {
        let store = self.shared.store(&self.profile);
        self.policy = store
            .zip(session.parse().ok())
            .and_then(|(store, id)| store.session_policy(id).ok().flatten())
            .and_then(|text| serde_json::from_str(&text).ok());
    }

    /// `busy` with the frozen policy's overrides.
    pub(super) fn with_policy_busy(&self, mut busy: EffectiveBusy) -> EffectiveBusy {
        if let Some(over) = self.policy.as_ref().and_then(|p| p.busy.as_ref()) {
            busy.input_mode = over.input_mode.unwrap_or(busy.input_mode);
            busy.debounce_ms = over.debounce_ms.unwrap_or(busy.debounce_ms);
            busy.debounce_cap_ms = over.debounce_cap_ms.unwrap_or(busy.debounce_cap_ms);
            busy.ack = over.ack.unwrap_or(busy.ack);
            busy.max_queued = over.max_queued.unwrap_or(busy.max_queued);
        }
        busy
    }

    /// `/pause [reason|off|status]`.
    pub(super) fn pause_command(&self, routing: &Routing, args: &str) {
        let home = &self.shared.options.home;
        let args = args.trim();
        let strings = self.strings();
        let reply = match args {
            "off" => match pmt_core::pause::resume(home) {
                Ok(true) => pmt_i18n::t!(strings, "gateway.pause_lifted"),
                Ok(false) => pmt_i18n::t!(strings, "gateway.not_paused"),
                Err(err) => err.message().to_owned(),
            },
            "status" => match pmt_core::pause::active(home) {
                Some(pause) => pmt_i18n::t!(
                    strings,
                    "gateway.paused_since",
                    at = pause.at,
                    actor = pause.actor,
                    reason = pause.reason
                ),
                None => pmt_i18n::t!(strings, "gateway.not_paused"),
            },
            reason => {
                let actor = format!("{}:{}", routing.platform, routing.sender_id);
                let reason = (!reason.is_empty()).then_some(reason);
                match pmt_core::pause::pause(home, reason, &actor) {
                    Ok(pause) => pmt_i18n::t!(strings, "gateway.paused_now", reason = pause.reason),
                    Err(err) => err.message().to_owned(),
                }
            }
        };
        self.say(&reply);
    }

    /// `/sethome`: this chat (and thread) becomes the platform's home
    /// channel in the profile's `config.toml`.
    pub(super) fn set_home(&self, routing: &Routing) {
        let Some(scope) = self.config() else {
            return;
        };
        let base = format!("gateway.platforms.{}.home_channel", routing.platform);
        let chat_key = format!("{base}.chat_id");
        let thread_key = format!("{base}.thread_id");
        let chat = serde_json::to_string(&routing.chat_id).expect("a string serializes");
        let file = scope.profile().config_file();
        let written = match &routing.thread_id {
            Some(thread) => {
                let thread = serde_json::to_string(thread).expect("a string serializes");
                file.set_all(&[
                    (chat_key.as_str(), chat.as_str()),
                    (thread_key.as_str(), thread.as_str()),
                ])
            }
            None => file
                .set(&chat_key, &chat)
                .and_then(|()| file.unset(&thread_key).map(|_| ())),
        };
        match written {
            Ok(()) => {
                let label = match &routing.thread_id {
                    Some(thread) => format!("{}:{thread}", routing.chat_id),
                    None => routing.chat_id.clone(),
                };
                tracing::info!(
                    profile = self.profile,
                    platform = routing.platform,
                    "home channel set"
                );
                self.say(&pmt_i18n::t!(
                    self.strings(),
                    "gateway.home_set",
                    label = label
                ));
            }
            Err(err) => {
                let text =
                    pmt_i18n::t!(self.strings(), "gateway.home_not_set", why = err.message());
                self.say(&text);
            }
        }
    }

    /// `/policy [show|apply --now]`.
    pub(super) async fn policy_command(&mut self, routing: &Routing, args: &str) {
        let args: Vec<&str> = args.split_whitespace().collect();
        let strings = self.strings();
        match args.as_slice() {
            [] | ["show"] => {
                let frozen = describe(&strings, self.policy.as_ref());
                let current = describe(&strings, self.current_policy(routing).as_ref());
                self.say(&pmt_i18n::t!(
                    strings,
                    "gateway.policy_show",
                    frozen = frozen,
                    current = current
                ));
            }
            ["apply", "--now"] if self.running.is_some() => {
                self.say(strings.get("gateway.policy_busy"));
            }
            ["apply", "--now"] => {
                let previous = self.current_session();
                match self
                    .transition(routing, Some(ResetReason::Policy), previous)
                    .await
                {
                    Ok(_) => self.say(strings.get("gateway.policy_applied")),
                    Err(err) => self.say(err.message()),
                }
            }
            _ => self.say(strings.get("gateway.policy_usage")),
        }
    }
}

fn describe(strings: &pmt_i18n::Strings, policy: Option<&ChatPolicy>) -> String {
    let Some(policy) = policy else {
        return pmt_i18n::t!(strings, "gateway.policy_none");
    };
    let mut parts = Vec::new();
    if let Some(model) = &policy.model {
        parts.push(pmt_i18n::t!(strings, "gateway.policy_model", model = model));
    }
    if let Some(toolsets) = &policy.toolsets {
        let list = toolsets.join(", ");
        parts.push(pmt_i18n::t!(
            strings,
            "gateway.policy_toolsets",
            list = list
        ));
    }
    if let Some(skills) = &policy.skills {
        let list = skills.join(", ");
        parts.push(pmt_i18n::t!(strings, "gateway.policy_skills", list = list));
    }
    if policy.system_prompt_append.is_some() {
        parts.push(pmt_i18n::t!(strings, "gateway.policy_prompt"));
    }
    if let Some(cost) = policy.max_cost_usd_per_turn {
        parts.push(pmt_i18n::t!(strings, "gateway.policy_cost", cost = cost));
    }
    if parts.is_empty() {
        pmt_i18n::t!(strings, "gateway.policy_limits_only")
    } else {
        parts.join("; ")
    }
}

/// `policy` with the chat's model override (`/model` in this chat), which
/// wins over the policy's own model.
fn with_model(
    policy: Option<ChatPolicy>,
    model: Option<String>,
    routing: &Routing,
) -> Option<ChatPolicy> {
    let Some(model) = model else {
        return policy;
    };
    let mut policy = policy.unwrap_or_else(|| ChatPolicy {
        platform: routing.platform.clone(),
        chat_id: Some(routing.chat_id.clone()),
        ..ChatPolicy::default()
    });
    policy.model = Some(model);
    Some(policy)
}

/// `policy` with the adapter's session hints added: hinted skills join the
/// policy's, hinted toolsets replace them.
fn with_hints(
    policy: Option<ChatPolicy>,
    hints: crate::messaging::adapter::SessionHints,
    routing: &Routing,
) -> Option<ChatPolicy> {
    if hints.is_empty() {
        return policy;
    }
    let mut policy = policy.unwrap_or_else(|| ChatPolicy {
        platform: routing.platform.clone(),
        chat_id: Some(routing.chat_id.clone()),
        ..ChatPolicy::default()
    });
    let skills = policy.skills.get_or_insert_with(Vec::new);
    for skill in hints.skills {
        if !skills.contains(&skill) {
            skills.push(skill);
        }
    }
    if hints.toolsets.is_some() {
        policy.toolsets = hints.toolsets;
    }
    Some(policy)
}
