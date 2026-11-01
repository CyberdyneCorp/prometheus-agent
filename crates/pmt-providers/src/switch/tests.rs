//! `/model` scenarios (spec: model-providers, /model switch command; User
//! model aliases).

use std::collections::BTreeMap;
use std::time::Duration;

use pmt_config::Config;
use pmt_core::secret::SecretString;

use super::*;
use crate::registry::ProviderRegistry;
use crate::resolve::{CredentialSource, NoExtras, TcpProbe};

static PROBE: TcpProbe = TcpProbe {
    timeout: Duration::from_millis(200),
};

const NEEDS: Requirements = Requirements {
    tools: true,
    min_context_window: 64_000,
};

struct Env {
    config: Config,
    registry: ProviderRegistry,
    secrets: BTreeMap<String, SecretString>,
    catalog: Catalog,
}

impl Env {
    fn new(config: &str, secrets: &[&str]) -> Self {
        let config: Config = toml::from_str(config).unwrap();
        Self {
            registry: ProviderRegistry::for_profile(&config, Vec::new()).unwrap(),
            catalog: Catalog::new(&config, None),
            secrets: secrets
                .iter()
                .map(|n| {
                    (
                        (*n).to_owned(),
                        SecretString::new(format!("{n}-0123456789")),
                    )
                })
                .collect(),
            config,
        }
    }

    fn resolver(&self) -> Resolver<'_> {
        Resolver {
            registry: &self.registry,
            config: &self.config,
            secrets: &self.secrets,
            extras: &NoExtras,
            probe: &PROBE,
            ad_hoc: None,
        }
    }

    async fn route(&self, provider: &str, model: &str) -> Route {
        let target = Target {
            provider: provider.to_owned(),
            model: Some(model.to_owned()),
            ..Target::default()
        };
        self.resolver()
            .resolve_target(&target, &mut Trace::default())
            .await
            .unwrap()
    }

    async fn run(
        &self,
        text: &str,
        state: &mut SessionModel,
        current: &Route,
    ) -> Result<Switched, Error> {
        let resolver = self.resolver();
        let ctx = Context {
            resolver: &resolver,
            catalog: &self.catalog,
            current,
            needs: NEEDS,
        };
        switch(&Args::parse(text)?, state, &ctx).await
    }
}

#[tokio::test]
async fn invalid_target_leaves_the_route_unchanged() {
    let env = Env::new(
        "[model]\nprovider = \"anthropic\"\ndefault = \"claude-x\"\n",
        &["ANTHROPIC_API_KEY"],
    );
    let current = env.route("anthropic", "claude-x").await;
    let mut state = SessionModel::default();
    let err = env
        .run("deepseek/large", &mut state, &current)
        .await
        .unwrap_err();
    assert_eq!(err.code().code, "provider.missing_credential");
    // The next turn still uses the session's route.
    assert_eq!(state.next_turn(), (None, None));
}

#[tokio::test]
async fn session_switch_applies_from_the_next_turn_with_a_cache_break_notice() {
    let env = Env::new(
        "[model]\nprovider = \"anthropic\"\n",
        &["ANTHROPIC_API_KEY", "OPENROUTER_API_KEY"],
    );
    let current = env.route("anthropic", "claude-x").await;
    let mut state = SessionModel::default();
    let switched = env
        .run(
            "openrouter/some-model --reasoning high",
            &mut state,
            &current,
        )
        .await
        .unwrap();
    assert_eq!(switched.scope, Scope::Session);
    assert!(!switched.persists());
    assert!(
        switched.notice.contains("cache break"),
        "{}",
        switched.notice
    );
    for _ in 0..2 {
        let (target, reasoning) = state.next_turn();
        assert_eq!(target.unwrap().provider, "openrouter");
        assert_eq!(reasoning, Some(ReasoningLevel::High));
    }
    // Same route again: no cache break.
    let same = env
        .run("anthropic/claude-x", &mut state, &current)
        .await
        .unwrap();
    assert!(!same.notice.contains("cache break"), "{}", same.notice);
}

#[tokio::test]
async fn once_applies_to_the_next_turn_only() {
    let env = Env::new(
        "[model]\nprovider = \"anthropic\"\n",
        &["ANTHROPIC_API_KEY", "DEEPSEEK_API_KEY"],
    );
    let current = env.route("anthropic", "claude-x").await;
    let mut state = SessionModel::default();
    env.run("deepseek/deepseek-chat --once", &mut state, &current)
        .await
        .unwrap();
    assert_eq!(state.next_turn().0.unwrap().provider, "deepseek");
    assert_eq!(state.next_turn(), (None, None));
}

#[tokio::test]
async fn first_pick_on_a_fresh_profile_persists() {
    let env = Env::new("", &["OPENROUTER_API_KEY"]);
    let current = env.route("openrouter", "x").await;
    let mut state = SessionModel::default();
    let switched = env
        .run("openrouter/some-model", &mut state, &current)
        .await
        .unwrap();
    assert_eq!(switched.scope, Scope::Global);
    assert!(switched.persists());
}

#[tokio::test]
async fn alias_switches_provider_and_key() {
    let env = Env::new(
        "[model]\nprovider = \"nous\"\n\
         [model.aliases.fav]\nprovider = \"openrouter\"\nmodel = \"anthropic/claude-x\"\n",
        &["NOUS_API_KEY", "OPENROUTER_API_KEY"],
    );
    let current = env.route("nous", "Hermes-4-405B").await;
    let mut state = SessionModel::default();
    let switched = env.run("FAV", &mut state, &current).await.unwrap();
    assert_eq!(switched.route.label(), "openrouter/anthropic/claude-x");
    assert_eq!(
        switched.route.credential.source,
        CredentialSource::Secret {
            name: "OPENROUTER_API_KEY".to_owned()
        }
    );
}

#[tokio::test]
async fn a_route_without_tools_or_with_a_small_window_is_refused() {
    let env = Env::new(
        "[model]\nprovider = \"anthropic\"\n\
         [models.\"deepseek/notools\"]\nsupports_tools = false\n\
         [models.\"deepseek/tiny\"]\ncontext_window = 8192\n",
        &["ANTHROPIC_API_KEY", "DEEPSEEK_API_KEY"],
    );
    let current = env.route("anthropic", "claude-x").await;
    let mut state = SessionModel::default();
    let err = env
        .run("deepseek/notools", &mut state, &current)
        .await
        .unwrap_err();
    assert_eq!(err.code().code, "model.capability_missing");
    let err = env
        .run("deepseek/tiny", &mut state, &current)
        .await
        .unwrap_err();
    assert_eq!(err.code().code, "model.context_too_small");
    assert_eq!(state, SessionModel::default());
}

#[test]
fn argument_rules() {
    let err = Args::parse("x --once --global").unwrap_err();
    assert_eq!(
        err.message(),
        "/model --once cannot be combined with --global"
    );
    assert_eq!(err.exit_code(), 2);
    let args = Args::parse("gpt-5 --provider openai --reasoning xhigh --session").unwrap();
    assert_eq!(args.reference.as_deref(), Some("gpt-5"));
    assert_eq!(args.provider.as_deref(), Some("openai"));
    assert_eq!(args.reasoning, Some(ReasoningLevel::Xhigh));
    assert_eq!(args.scope, Some(Scope::Session));
    assert!(Args::parse("x --reasoning loud").is_err());
    assert!(Args::parse("x --bogus").is_err());
    assert!(Args::parse("x y").is_err());
}
