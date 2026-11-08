//! Data-use confirmation for training-flagged models (spec:
//! model-providers, Data-use confirmation for training-flagged models).
//!
//! Some models train on what is sent to them (Meta AI's contributor-tier
//! `muse-spark-*-contributor` models). Such a model is never selected,
//! by any surface or slot, until its data-use terms are accepted: on a
//! terminal by confirming, otherwise by `[models."<provider>/<model>"]
//! accept_data_use = true`. Resolution refuses an unconfirmed one, so a
//! fallback, auxiliary or mixture slot cannot reach it either.

use pmt_config::Config;
use pmt_config::schema::ModelEntry;
use pmt_core::error::{Error, codes};

use crate::descriptor::ModelWire;

/// The terms a confirmation names.
pub const TERMS: &str = "Meta AI contributor terms: the prompts, files and outputs of this model are used to train Meta's models";

/// Built-in training-flagged models: `(provider, model pattern)`.
const FLAGGED: &[(&str, &str)] = &[("meta-ai", "muse-spark-*-contributor")];

/// Whether `provider`/`model` trains on the data sent to it (the
/// `trains_on_data` metadata flag).
#[must_use]
pub fn trains_on_data(provider: &str, model: &str) -> bool {
    FLAGGED.iter().any(|(p, pattern)| {
        *p == provider
            && ModelWire {
                pattern: (*pattern).to_owned(),
                wire_format: pmt_config::schema::WireFormat::ChatCompletions,
            }
            .matches(model)
    })
}

/// Whether the profile accepted `provider`/`model`'s data-use terms.
#[must_use]
pub fn accepted(config: &Config, provider: &str, model: &str) -> bool {
    matches!(
        config.models.get(&format!("{provider}/{model}")),
        Some(ModelEntry::Route(route)) if route.accept_data_use == Some(true)
    )
}

/// Refuses an unconfirmed training-flagged model.
///
/// # Errors
///
/// `model.data_use_unconfirmed` naming the terms (`prometheus model set`
/// exits 4 with it).
pub fn check(config: &Config, provider: &str, model: &str) -> Result<(), Error> {
    if !trains_on_data(provider, model) || accepted(config, provider, model) {
        return Ok(());
    }
    Err(unconfirmed(provider, model))
}

/// `model.data_use_unconfirmed` for `provider`/`model`.
#[must_use]
pub fn unconfirmed(provider: &str, model: &str) -> Error {
    Error::new(
        &codes::MODEL_DATA_USE_UNCONFIRMED,
        format!("{provider}/{model} trains on your data ({TERMS}); its data-use terms are not accepted"),
    )
    .with_hint(format!(
        "accept them on a terminal (prometheus model set {provider}/{model}), or set [models.\"{provider}/{model}\"] accept_data_use = true"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contributor_models_need_accepted_terms() {
        let model = "muse-spark-1.2-contributor";
        assert!(trains_on_data("meta-ai", model));
        assert!(!trains_on_data("meta-ai", "muse-spark-1.2"));
        let err = check(&Config::default(), "meta-ai", model).unwrap_err();
        assert_eq!(err.code().code, "model.data_use_unconfirmed");
        assert!(err.message().contains("Meta AI contributor terms"));
        let config: Config = toml::from_str(&format!(
            "[models.\"meta-ai/{model}\"]\naccept_data_use = true\n"
        ))
        .unwrap();
        check(&config, "meta-ai", model).unwrap();
    }
}
