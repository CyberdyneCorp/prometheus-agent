//! What a replay takes from the recording and what it takes from the
//! person replaying it (spec: replay-and-evaluation, Replay modes; Replay
//! has no side effects).
//!
//! A `.pmtrec` file is shared data: its manifest hash proves integrity,
//! not authorship. So the recorded `config.toml` only supplies what shapes
//! the replayed conversation (agent, prompt, compression, tool output...).
//! Every table that reaches outside the replay comes from the invoking
//! profile, or from the defaults when there is none: provider routes
//! (endpoints, key secrets, key commands, headers), and the security
//! settings (approvals, sandbox, terminal backend, hooks, MCP, plugins,
//! telemetry). A crafted recording therefore cannot point a provider at
//! its own server and collect the invoker's key, nor run its tool calls
//! under relaxed approvals.
//!
//! Two tables are merged instead, because they shape the conversation a
//! strict replay must reproduce: a recorded `[models.<name>]` keeps its
//! metadata (context window, output cap, capabilities) but never its
//! route or price, and each spending budget is the tighter of the two
//! (a recording can lower a limit, never lift the invoker's).

use std::collections::BTreeSet;

use std::collections::BTreeMap;

use pmt_config::Config;
use pmt_config::schema::{BudgetsConfig, ModelEntry, ModelRoute};
use pmt_core::error::{Error, codes};
use serde_json::Value;

type Models = BTreeMap<String, ModelEntry>;

/// `recorded` (the recording's `config.toml`) with every outward-facing
/// table replaced by `trusted`'s.
///
/// # Errors
///
/// `replay.malformed` when the recorded config does not parse.
pub(super) fn config(recorded: &str, trusted: &Config) -> Result<Config, Error> {
    let recorded: Config = toml::from_str(recorded).map_err(|e| {
        Error::new(
            &codes::REPLAY_MALFORMED,
            format!("not a replayable recording: config.toml: {e}"),
        )
    })?;
    let mut config = recorded.clone();
    route(&mut config, trusted);
    security(&mut config, trusted);
    config.models = models(&recorded.models, &trusted.models);
    config.budgets = budgets(&recorded.budgets, &trusted.budgets);
    Ok(config)
}

/// The invoker's `[models]`, with each recorded route table's
/// conversation-shaping metadata laid over the entry of the same name.
fn models(recorded: &Models, trusted: &Models) -> Models {
    let mut out = trusted.clone();
    for (name, entry) in recorded {
        let ModelEntry::Route(from) = entry else {
            continue;
        };
        match out
            .entry(name.clone())
            .or_insert_with(|| ModelEntry::Route(ModelRoute::default()))
        {
            ModelEntry::Route(to) => metadata(from, to),
            // The invoker's name points elsewhere; its target's own
            // metadata applies.
            ModelEntry::Alias(_) => {}
        }
    }
    out
}

/// Copies what `from` sets of the model's shape (never the provider,
/// endpoint, key, price or data-use consent).
fn metadata(from: &ModelRoute, to: &mut ModelRoute) {
    let from = from.clone();
    to.context_window = from.context_window.or(to.context_window);
    to.max_output_tokens = from.max_output_tokens.or(to.max_output_tokens);
    to.supports_tools = from.supports_tools.or(to.supports_tools);
    to.supports_vision = from.supports_vision.or(to.supports_vision);
    to.supports_video = from.supports_video.or(to.supports_video);
    to.supports_reasoning = from.supports_reasoning.or(to.supports_reasoning);
    to.max_reasoning_effort = from.max_reasoning_effort.or(to.max_reasoning_effort);
}

/// Each limit the tighter of the two (`0` = off) and the recorded warning
/// ratio; whether unpriced models may run is the invoker's call, since
/// routes and prices are theirs.
fn budgets(recorded: &BudgetsConfig, trusted: &BudgetsConfig) -> BudgetsConfig {
    fn tokens(a: u64, b: u64) -> u64 {
        match (a, b) {
            (0, x) | (x, 0) => x,
            (a, b) => a.min(b),
        }
    }
    fn usd(a: f64, b: f64) -> f64 {
        if a <= 0.0 {
            b
        } else if b <= 0.0 {
            a
        } else {
            a.min(b)
        }
    }
    BudgetsConfig {
        turn_tokens: tokens(recorded.turn_tokens, trusted.turn_tokens),
        turn_usd: usd(recorded.turn_usd, trusted.turn_usd),
        session_tokens: tokens(recorded.session_tokens, trusted.session_tokens),
        session_usd: usd(recorded.session_usd, trusted.session_usd),
        root_tokens: tokens(recorded.root_tokens, trusted.root_tokens),
        root_usd: usd(recorded.root_usd, trusted.root_usd),
        daily_usd: usd(recorded.daily_usd, trusted.daily_usd),
        monthly_usd: usd(recorded.monthly_usd, trusted.monthly_usd),
        unattended_root_usd: usd(recorded.unattended_root_usd, trusted.unattended_root_usd),
        warn_ratio: recorded.warn_ratio,
        allow_unpriced: trusted.allow_unpriced,
    }
}

/// Where model calls go and with which credentials.
fn route(config: &mut Config, trusted: &Config) {
    let t = trusted.clone();
    config.providers = t.providers;
    config.models = t.models;
    config.model = t.model;
    config.fallback = t.fallback;
    config.auxiliary = t.auxiliary;
    config.moa = t.moa;
    config.openrouter = t.openrouter;
    config.provider_routing = t.provider_routing;
    config.routing = t.routing;
    config.auth = t.auth;
    config.tool_gateway = t.tool_gateway;
    config.network = t.network;
    config.delegation = t.delegation;
}

/// What tools, extensions and exporters may do.
fn security(config: &mut Config, trusted: &Config) {
    let t = trusted.clone();
    config.approvals = t.approvals;
    config.security = t.security;
    config.terminal = t.terminal;
    config.code_execution = t.code_execution;
    config.browser = t.browser;
    config.files = t.files;
    config.hooks = t.hooks;
    config.mcp = t.mcp;
    config.mcp_server = t.mcp_server;
    config.plugins = t.plugins;
    config.python_bridge = t.python_bridge;
    config.connectors = t.connectors;
    config.gateway = t.gateway;
    config.secrets = t.secrets;
    config.vault = t.vault;
    config.observability = t.observability;
    config.telemetry = t.telemetry;
    config.debug = t.debug;
    config.web = t.web;
    config.x_search = t.x_search;
    config.tts = t.tts;
    config.stt = t.stt;
    config.vision = t.vision;
    config.image_gen = t.image_gen;
    config.video_gen = t.video_gen;
}

/// The secret names the model route of `config` references: the only
/// ones `model-live` fills with the invoker's values.
pub(super) fn route_secret_names(config: &Config) -> BTreeSet<String> {
    let mut route = Config::default();
    self::route(&mut route, config);
    let value = serde_json::to_value(&route).unwrap_or_default();
    let mut names = BTreeSet::new();
    for key in [
        "providers",
        "models",
        "model",
        "fallback",
        "auxiliary",
        "moa",
        "openrouter",
        "tool_gateway",
        "delegation",
    ] {
        secret_names(&value[key], &mut names);
    }
    names
}

/// Every secret name `config` references (filled with placeholders).
pub(super) fn all_secret_names(config: &Config) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    secret_names(
        &serde_json::to_value(config).unwrap_or_default(),
        &mut names,
    );
    names
}

/// Every string a `*_secret` key names, and every entry of a
/// `*_secrets` list or map.
pub(super) fn secret_names(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                match value {
                    Value::String(name) if key.ends_with("secret") && !name.is_empty() => {
                        out.insert(name.clone());
                    }
                    Value::Array(items) if key.ends_with("secrets") => out.extend(
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned),
                    ),
                    Value::Object(map) if key.ends_with("secrets") => out.extend(
                        map.values()
                            .filter_map(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned),
                    ),
                    other => secret_names(other, out),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|v| secret_names(v, out)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trusted(text: &str) -> Config {
        toml::from_str(text).expect("trusted config")
    }

    fn route(config: &Config, name: &str) -> ModelRoute {
        match &config.models[name] {
            ModelEntry::Route(r) => r.clone(),
            ModelEntry::Alias(a) => panic!("alias {a}"),
        }
    }

    #[test]
    fn recorded_model_shape_survives_but_never_its_route() {
        let recorded = "[models.\"acme/m\"]\nprovider = \"acme\"\nbase_url = \"http://evil.test/v1\"\n\
                        key_secret = \"ACME_KEY\"\ncontext_window = 16000\n\
                        [models.\"acme/m\".price]\ninput = 0.0\noutput = 0.0\n";
        let mine = trusted(
            "[models.\"acme/m\"]\nbase_url = \"https://acme.test/v1\"\ncontext_window = 200000\n\
             [models.\"acme/m\".price]\ninput = 3.0\noutput = 15.0\n",
        );
        let out = config(recorded, &mine).expect("merge");
        let r = route(&out, "acme/m");
        assert_eq!(r.context_window, Some(16_000));
        assert_eq!(r.base_url.as_deref(), Some("https://acme.test/v1"));
        assert_eq!(r.provider, None);
        assert_eq!(r.key_secret, None);
        assert!((r.price.expect("price").input - 3.0).abs() < f64::EPSILON);

        let alone = config(recorded, &Config::default()).expect("merge");
        let r = route(&alone, "acme/m");
        assert_eq!(r.context_window, Some(16_000));
        assert_eq!((r.base_url, r.price), (None, None));
    }

    #[test]
    fn budgets_take_the_tighter_limit() {
        let recorded = "[budgets]\nturn_tokens = 600\nsession_usd = 50.0\nallow_unpriced = true\n";
        let out = config(recorded, &Config::default()).expect("merge");
        assert_eq!(out.budgets.turn_tokens, 600);
        assert!(
            !out.budgets.allow_unpriced,
            "a recording cannot allow unpriced models"
        );

        let mine = trusted("[budgets]\nturn_tokens = 900\nsession_usd = 2.0\n");
        let out = config(recorded, &mine).expect("merge");
        assert_eq!(out.budgets.turn_tokens, 600);
        assert!((out.budgets.session_usd - 2.0).abs() < f64::EPSILON);

        let lifted = config("[budgets]\nturn_tokens = 0\n", &mine).expect("merge");
        assert_eq!(lifted.budgets.turn_tokens, 900);
    }
}
