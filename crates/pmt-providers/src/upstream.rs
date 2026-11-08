//! OpenRouter upstream provider routing (spec: model-providers, OpenRouter
//! provider routing preferences).
//!
//! `[provider_routing]` (with per-model overrides) becomes the request's
//! `provider` object, and the `:nitro` / `:floor` model suffixes become a
//! throughput / price sort with the suffix removed from the sent model.
//! The `openrouter/pareto-code` router also gets its `pareto-router`
//! plugin block. All of it is computed when a route resolves, so it
//! follows the route wherever it is used (fallback entries, cron jobs,
//! delegated children) and is never sent to another provider.

use std::sync::Arc;

use pmt_config::Config;
use pmt_config::schema::{RoutingPrefs, RoutingSort};
use serde_json::{Map, Value, json};

/// The provider these settings belong to.
pub const OPENROUTER: &str = "openrouter";

/// The router model the coding score applies to.
pub const PARETO_MODEL: &str = "openrouter/pareto-code";

/// The plugin id of the pareto router.
pub const PARETO_PLUGIN: &str = "pareto-router";

/// The top-level body fields a route on `provider` adds to every request
/// for `model`; `None` for other providers or when nothing is set.
#[must_use]
pub fn extras(config: &Config, provider: &str, model: &str) -> Option<Arc<Map<String, Value>>> {
    if provider != OPENROUTER {
        return None;
    }
    let (sent_model, suffix_sort) = split_suffix(model);
    let mut prefs = config.provider_routing.prefs();
    if let Some(over) = model_override(config, sent_model) {
        prefs = prefs.overlaid(over);
    }
    if let Some(sort) = suffix_sort {
        prefs.sort = Some(sort);
    }
    let mut body = Map::new();
    if sent_model != model {
        body.insert("model".to_owned(), json!(sent_model));
    }
    if !prefs.is_empty() {
        body.insert("provider".to_owned(), provider_object(&prefs));
    }
    if is_pareto(sent_model)
        && let Some(score) = config.openrouter.min_coding_score
    {
        body.insert(
            "plugins".to_owned(),
            json!([{ "id": PARETO_PLUGIN, "min_coding_score": score }]),
        );
    }
    (!body.is_empty()).then(|| Arc::new(body))
}

/// The model without a `:nitro` / `:floor` suffix, and the sort the
/// suffix asks for.
fn split_suffix(model: &str) -> (&str, Option<RoutingSort>) {
    if let Some(base) = model.strip_suffix(":nitro") {
        return (base, Some(RoutingSort::Throughput));
    }
    if let Some(base) = model.strip_suffix(":floor") {
        return (base, Some(RoutingSort::Price));
    }
    (model, None)
}

fn is_pareto(model: &str) -> bool {
    model.eq_ignore_ascii_case(PARETO_MODEL) || model.eq_ignore_ascii_case("pareto-code")
}

/// The `[provider_routing.models."<m>"]` table of `model`: names compare
/// case-insensitively with separators (`/ - _ . :` and spaces) ignored.
fn model_override<'a>(config: &'a Config, model: &str) -> Option<&'a RoutingPrefs> {
    let wanted = squash(model);
    config
        .provider_routing
        .models
        .iter()
        .find(|(name, _)| squash(name) == wanted)
        .map(|(_, prefs)| prefs)
}

fn squash(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn provider_object(prefs: &RoutingPrefs) -> Value {
    let mut object = Map::new();
    if let Some(sort) = prefs.sort {
        object.insert("sort".to_owned(), json!(sort.as_str()));
    }
    for (key, list) in [
        ("only", &prefs.only),
        ("ignore", &prefs.ignore),
        ("order", &prefs.order),
    ] {
        if !list.is_empty() {
            object.insert(key.to_owned(), json!(list));
        }
    }
    if let Some(require) = prefs.require_parameters {
        object.insert("require_parameters".to_owned(), json!(require));
    }
    if let Some(data) = prefs.data_collection {
        object.insert("data_collection".to_owned(), json!(data.as_str()));
    }
    Value::Object(object)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn preferences_map_to_the_provider_object_on_openrouter_only() {
        let c = config(
            "[provider_routing]\nsort = \"latency\"\nonly = [\"anthropic\"]\ndata_collection = \"deny\"\n",
        );
        let body = extras(&c, "openrouter", "anthropic/claude-x").unwrap();
        assert_eq!(
            body["provider"],
            json!({"sort": "latency", "only": ["anthropic"], "data_collection": "deny"})
        );
        assert!(!body.contains_key("model"));
        assert!(extras(&c, "anthropic", "claude-x").is_none());
        assert!(extras(&Config::default(), "openrouter", "x/y").is_none());
    }

    #[test]
    fn per_model_overrides_match_ignoring_case_and_separators() {
        let c = config(
            "[provider_routing]\nsort = \"price\"\n\
             [provider_routing.models.\"Anthropic/Claude Sonnet 4.5\"]\nsort = \"throughput\"\n",
        );
        let hit = extras(&c, "openrouter", "anthropic/claude-sonnet-4.5").unwrap();
        assert_eq!(hit["provider"]["sort"], "throughput");
        let miss = extras(&c, "openrouter", "openai/gpt-5").unwrap();
        assert_eq!(miss["provider"]["sort"], "price");
    }

    #[test]
    fn nitro_and_floor_suffixes_become_sorts_and_leave_the_sent_model() {
        let c = Config::default();
        let nitro = extras(&c, "openrouter", "meta/llama:nitro").unwrap();
        assert_eq!(nitro["model"], "meta/llama");
        assert_eq!(nitro["provider"]["sort"], "throughput");
        let floor = extras(&c, "openrouter", "meta/llama:floor").unwrap();
        assert_eq!(floor["provider"]["sort"], "price");
    }

    #[test]
    fn the_pareto_block_needs_the_router_model_and_a_score() {
        let c = config("[openrouter]\nmin_coding_score = 0.8\n");
        let pareto = extras(&c, "openrouter", PARETO_MODEL).unwrap();
        assert_eq!(
            pareto["plugins"],
            json!([{"id": "pareto-router", "min_coding_score": 0.8}])
        );
        assert!(extras(&c, "openrouter", "anthropic/claude-x").is_none());
        let empty = config("[openrouter]\nmin_coding_score = \"\"\n");
        assert!(extras(&empty, "openrouter", PARETO_MODEL).is_none());
    }
}
