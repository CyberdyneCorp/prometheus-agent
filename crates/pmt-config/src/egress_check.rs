//! Checks of `[security.egress]` credential injection (spec:
//! security-boundaries, Credential injection at the egress proxy): each
//! `security.egress.inject` key names a secret and maps it to at least one
//! host pattern.

use pmt_core::error::{Error, codes};

use crate::schema::{Config, is_secret_name};

/// Fails with `config.invalid` naming the first bad mapping.
///
/// # Errors
///
/// `config.invalid` (exit 78).
pub fn check(config: &Config) -> Result<(), Error> {
    for (name, hosts) in &config.security.egress.inject {
        if !is_secret_name(name) {
            return Err(Error::new(
                &codes::CONFIG_INVALID,
                format!(
                    "security.egress.inject.{name}: not a secret name (^[A-Z][A-Z0-9_]{{0,127}}$)"
                ),
            ));
        }
        if hosts.iter().any(|h| h.trim().is_empty()) {
            return Err(Error::new(
                &codes::CONFIG_INVALID,
                format!("security.egress.inject.{name}: a host pattern is empty"),
            ));
        }
    }
    Ok(())
}

/// The hosts injected credentials may go to when a mapping names none
/// (spec: security-boundaries, Egress proxy coverage of containers and
/// remote sandboxes).
pub const PROVIDER_HOSTS: &[&str] = &[
    "openrouter.ai",
    "*.openrouter.ai",
    "api.openai.com",
    "api.anthropic.com",
    "generativelanguage.googleapis.com",
    "api.x.ai",
    "api.mistral.ai",
    "api.groq.com",
    "api.together.xyz",
    "api.deepseek.com",
    "inference.nousresearch.com",
];

/// The host patterns secret `name` may be injected toward: its mapping's,
/// or for an empty mapping the built-in provider hosts plus
/// `security.egress.inject_extra_hosts`.
#[must_use]
pub fn inject_hosts(config: &Config, name: &str) -> Vec<String> {
    let egress = &config.security.egress;
    match egress.inject.get(name) {
        Some(hosts) if !hosts.is_empty() => hosts.clone(),
        _ => PROVIDER_HOSTS
            .iter()
            .map(|h| (*h).to_owned())
            .chain(egress.inject_extra_hosts.iter().cloned())
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn mappings_need_a_secret_name_and_no_blank_host() {
        assert!(
            check(&config(
                "[security.egress.inject]\nGITHUB_TOKEN = [\"api.github.com\"]\n"
            ))
            .is_ok()
        );
        let err = check(&config("[security.egress.inject]\ngithub = [\"x\"]\n")).unwrap_err();
        assert!(
            err.message().contains("not a secret name"),
            "{}",
            err.message()
        );
        let err = check(&config(
            "[security.egress.inject]\nGITHUB_TOKEN = [\" \"]\n",
        ))
        .unwrap_err();
        assert!(err.message().contains("is empty"), "{}", err.message());
    }

    #[test]
    fn an_empty_mapping_means_the_provider_hosts_and_the_extras() {
        let config = config(
            "[security.egress]\ninject_extra_hosts = [\"llm.internal\"]\n[security.egress.inject]\nOPENAI_API_KEY = []\nGH = [\"api.github.com\"]\n",
        );
        assert!(check(&config).is_ok());
        let hosts = inject_hosts(&config, "OPENAI_API_KEY");
        assert!(hosts.contains(&"api.openai.com".to_owned()));
        assert!(hosts.contains(&"llm.internal".to_owned()));
        assert_eq!(inject_hosts(&config, "GH"), ["api.github.com"]);
    }
}
