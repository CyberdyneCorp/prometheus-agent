//! Prices and the pricing rule (spec: observability, Pricing and unpriced
//! calls; model-providers, cost status): the provider-reported cost when
//! present, else the model's price table, else unpriced (`cost_usd` null,
//! never zero).

use std::collections::BTreeMap;

use pmt_config::schema::{Config, ModelEntry, ModelPrice};
use pmt_store::{CostStatus, Tokens};

/// Per-million-token USD rates of one model.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Price {
    /// Uncached input tokens.
    pub input: f64,
    /// Output tokens.
    pub output: f64,
    /// Cache-read tokens.
    pub cache_read: f64,
    /// Cache-write tokens.
    pub cache_write: f64,
    /// Reasoning tokens; `None` bills them at the output rate.
    pub reasoning: Option<f64>,
}

const PER_TOKEN: f64 = 1_000_000.0;

#[allow(clippy::cast_precision_loss)] // token counts stay far below 2^52
fn usd(tokens: u64, rate: f64) -> f64 {
    tokens as f64 * rate / PER_TOKEN
}

impl Price {
    /// The cost of `tokens`. Reasoning tokens are part of the output
    /// tokens (as providers report them), so only the visible remainder is
    /// billed at the output rate.
    #[must_use]
    pub fn cost(&self, tokens: &Tokens) -> f64 {
        let reasoning = tokens.reasoning.min(tokens.output);
        usd(tokens.input, self.input)
            + usd(tokens.output - reasoning, self.output)
            + usd(reasoning, self.reasoning_rate())
            + usd(tokens.cache_read, self.cache_read)
            + usd(tokens.cache_write, self.cache_write)
    }

    /// Worst case of a request (spec: Hard budgets): every input token at
    /// the input rate and the full output allowance at the dearer of the
    /// output and reasoning rates.
    #[must_use]
    pub fn worst_case(&self, input_tokens: u64, max_output_tokens: u64) -> f64 {
        usd(input_tokens, self.input)
            + usd(max_output_tokens, self.output.max(self.reasoning_rate()))
    }

    fn reasoning_rate(&self) -> f64 {
        self.reasoning.unwrap_or(self.output)
    }
}

impl From<&ModelPrice> for Price {
    fn from(price: &ModelPrice) -> Self {
        Self {
            input: price.input,
            output: price.output,
            cache_read: price.cache_read,
            cache_write: price.cache_write,
            reasoning: price.reasoning,
        }
    }
}

/// Where model prices come from. The config table is the M1 source; the
/// provider catalog (model-providers) plugs in behind the same trait.
pub trait Prices: Send + Sync {
    /// The rates of `provider`/`model`, `None` when unknown.
    fn price(&self, provider: &str, model: &str) -> Option<Price>;
}

/// Prices from `[models.<name>.price]` in the profile config, keyed by
/// `"<provider>/<model>"`: an entry named that way, or any entry whose
/// route names that provider and model.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigPrices {
    by_route: BTreeMap<String, Price>,
}

impl ConfigPrices {
    /// Collects the priced `[models]` entries of `config`.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let mut by_route = BTreeMap::new();
        for (name, entry) in &config.models {
            let ModelEntry::Route(route) = entry else {
                continue;
            };
            let Some(price) = &route.price else {
                continue;
            };
            if let (Some(provider), Some(model)) = (&route.provider, &route.model) {
                by_route.insert(format!("{provider}/{model}"), Price::from(price));
            }
            // The entry's own name wins over a route naming the same model.
            by_route.insert(name.clone(), Price::from(price));
        }
        Self { by_route }
    }
}

impl Prices for ConfigPrices {
    fn price(&self, provider: &str, model: &str) -> Option<Price> {
        self.by_route.get(&format!("{provider}/{model}")).copied()
    }
}

/// What the provider adapter knows about a finished call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Billing<'a> {
    /// Tokens by class.
    pub tokens: &'a Tokens,
    /// The cost the provider reported, if any.
    pub reported_usd: Option<f64>,
    /// The call's billing source (`subscription:...`, `api_key:...`...).
    pub billing_source: &'a str,
    /// Served from the replay cache.
    pub cache_hit: bool,
}

/// The pricing rule: a replay-cache hit costs 0; a subscription call costs
/// 0 with status `subscription`; then the provider's reported cost
/// (`exact`), the price table (`estimated`), else unpriced (`unknown`,
/// `None`).
#[must_use]
pub fn price_call(billing: &Billing<'_>, price: Option<&Price>) -> (Option<f64>, CostStatus) {
    if billing.cache_hit {
        return (Some(0.0), CostStatus::Exact);
    }
    if billing.billing_source.starts_with("subscription:") {
        return (Some(0.0), CostStatus::Subscription);
    }
    if let Some(reported) = billing.reported_usd {
        return (Some(reported), CostStatus::Exact);
    }
    match price {
        Some(price) => (Some(price.cost(billing.tokens)), CostStatus::Estimated),
        None => (None, CostStatus::Unknown),
    }
}

#[cfg(test)]
mod tests {
    use pmt_config::schema::{Config, ModelEntry, ModelPrice, ModelRoute};
    use pmt_store::{CostStatus, Tokens};

    use super::{Billing, ConfigPrices, Price, Prices, price_call};

    fn tokens() -> Tokens {
        Tokens {
            input: 1_000_000,
            output: 200_000,
            cache_read: 500_000,
            cache_write: 0,
            reasoning: 100_000,
        }
    }

    fn price() -> Price {
        Price {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
            reasoning: Some(20.0),
        }
    }

    fn billing<'a>(tokens: &'a Tokens, source: &'a str, reported: Option<f64>) -> Billing<'a> {
        Billing {
            tokens,
            reported_usd: reported,
            billing_source: source,
            cache_hit: false,
        }
    }

    #[test]
    fn cost_bills_reasoning_as_part_of_output() {
        // 3.00 input + 100k visible * 15 + 100k reasoning * 20 + 0.15 cache.
        let cost = price().cost(&tokens());
        assert!((cost - (3.0 + 1.5 + 2.0 + 0.15)).abs() < 1e-9, "{cost}");
        let no_reasoning_rate = Price {
            reasoning: None,
            ..price()
        };
        assert!((no_reasoning_rate.cost(&tokens()) - (3.0 + 3.0 + 0.15)).abs() < 1e-9);
        assert!((price().worst_case(100_000, 10_000) - (0.3 + 0.2)).abs() < 1e-9);
    }

    #[test]
    fn provider_cost_then_price_table_then_unpriced() {
        let t = tokens();
        assert_eq!(
            price_call(&billing(&t, "api_key:openai", Some(0.42)), Some(&price())),
            (Some(0.42), CostStatus::Exact)
        );
        let (cost, status) = price_call(&billing(&t, "api_key:openai", None), Some(&price()));
        assert_eq!(status, CostStatus::Estimated);
        assert!((cost.unwrap() - 6.65).abs() < 1e-9);
        assert_eq!(
            price_call(&billing(&t, "local", None), None),
            (None, CostStatus::Unknown)
        );
        assert_eq!(
            price_call(&billing(&t, "subscription:anthropic/work", Some(1.0)), None),
            (Some(0.0), CostStatus::Subscription)
        );
        let cached = Billing {
            cache_hit: true,
            ..billing(&t, "api_key:openai", Some(1.0))
        };
        assert_eq!(price_call(&cached, None), (Some(0.0), CostStatus::Exact));
    }

    #[test]
    fn config_prices_are_found_by_entry_name_and_by_route() {
        let mut config = Config::default();
        let model_price = ModelPrice {
            input: 1.0,
            output: 2.0,
            ..ModelPrice::default()
        };
        config.models.insert(
            "openai/gpt-x".to_owned(),
            ModelEntry::Route(ModelRoute {
                price: Some(model_price.clone()),
                ..ModelRoute::default()
            }),
        );
        config.models.insert(
            "fast".to_owned(),
            ModelEntry::Route(ModelRoute {
                provider: Some("acme".to_owned()),
                model: Some("swift-1".to_owned()),
                price: Some(model_price),
                ..ModelRoute::default()
            }),
        );
        config.models.insert(
            "alias".to_owned(),
            ModelEntry::Alias("ollama/llama".to_owned()),
        );
        let prices = ConfigPrices::from_config(&config);
        let gpt = prices.price("openai", "gpt-x").unwrap();
        assert!((gpt.output - 2.0).abs() < f64::EPSILON);
        let swift = prices.price("acme", "swift-1").unwrap();
        assert!((swift.input - 1.0).abs() < f64::EPSILON);
        assert_eq!(prices.price("ollama", "llama"), None);
    }
}
