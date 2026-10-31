//! `/model` and `/reasoning` bodies against a real profile (spec:
//! model-providers, /model switch command; Reasoning effort defaults,
//! overrides and display).

use std::path::Path;
use std::time::Duration;

use pmt_config::schema::{ReasoningLevel, SecretsBackend};
use pmt_config::secrets::SecretStore;
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::secret::SecretString;
use pmt_providers::ProviderRegistry;
use pmt_providers::metadata::{Catalog, ModelMetadata};
use pmt_providers::resolve::{NoExtras, Resolver, Route, Target, TcpProbe, Trace};
use pmt_providers::switch::{Context, Requirements, SessionModel};
use prometheus::model_slash;

static PROBE: TcpProbe = TcpProbe {
    timeout: Duration::from_millis(200),
};

fn scope(root: &Path, config: &str, secrets: &[&str]) -> ProfileScope {
    let home = Home::open(root).unwrap();
    let profile = home.profile(&ProfileName::default_profile()).unwrap();
    std::fs::write(profile.config_file().path(), config).unwrap();
    let store = SecretStore::open(&profile, SecretsBackend::File);
    for name in secrets {
        store
            .set(name, &SecretString::new(format!("{name}-value-0123456789")))
            .unwrap();
    }
    ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap()
}

async fn route(resolver: &Resolver<'_>, provider: &str, model: &str) -> Route {
    let target = Target {
        provider: provider.to_owned(),
        model: Some(model.to_owned()),
        ..Target::default()
    };
    resolver
        .resolve_target(&target, &mut Trace::default())
        .await
        .unwrap()
}

fn config_text(scope: &ProfileScope) -> String {
    std::fs::read_to_string(scope.profile().config_file().path()).unwrap()
}

#[tokio::test]
async fn model_switch_validates_before_changing_anything_and_persists_a_first_pick() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let scope = scope(dir.path(), "", &["ANTHROPIC_API_KEY", "OPENROUTER_API_KEY"]);
    let config = scope.config();
    let registry = ProviderRegistry::for_profile(config, Vec::new()).unwrap();
    let catalog = Catalog::new(config, None);
    let resolver = Resolver {
        registry: &registry,
        config,
        secrets: scope.secrets(),
        extras: &NoExtras,
        probe: &PROBE,
        ad_hoc: None,
    };
    let current = route(&resolver, "anthropic", "claude-x").await;
    let ctx = Context {
        resolver: &resolver,
        catalog: &catalog,
        current: &current,
        needs: Requirements {
            tools: true,
            min_context_window: 64_000,
        },
    };
    let mut session = SessionModel::default();
    let err = model_slash::model(&scope, &mut session, &ctx, "deepseek/large")
        .await
        .unwrap_err();
    assert_eq!(err.code().code, "provider.missing_credential");
    assert_eq!(session, SessionModel::default());
    assert_eq!(config_text(&scope), "");

    let err = model_slash::model(&scope, &mut session, &ctx, "x --once --global")
        .await
        .unwrap_err();
    assert_eq!(
        err.message(),
        "/model --once cannot be combined with --global"
    );

    // Fresh profile, no scope flag: written like --global.
    let notice = model_slash::model(&scope, &mut session, &ctx, "openrouter/some-model")
        .await
        .unwrap();
    assert!(notice.contains("cache break"), "{notice}");
    let written: toml::Table = toml::from_str(&config_text(&scope)).unwrap();
    assert_eq!(written["model"]["provider"].as_str(), Some("openrouter"));
    assert_eq!(written["model"]["default"].as_str(), Some("some-model"));
    assert_eq!(session.next_turn().0.unwrap().provider, "openrouter");
}

#[tokio::test]
async fn reasoning_levels_clamp_and_display_toggles_persist() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let scope = scope(dir.path(), "", &["OPENROUTER_API_KEY"]);
    let config = scope.config();
    let registry = ProviderRegistry::for_profile(config, Vec::new()).unwrap();
    let resolver = Resolver {
        registry: &registry,
        config,
        secrets: scope.secrets(),
        extras: &NoExtras,
        probe: &PROBE,
        ad_hoc: None,
    };
    let current = route(&resolver, "openrouter", "x").await;
    let meta = ModelMetadata {
        supports_reasoning: true,
        max_reasoning_effort: Some(ReasoningLevel::High),
        ..ModelMetadata::default()
    };
    let mut session = SessionModel::default();
    let text = model_slash::reasoning(&scope, &mut session, &current, &meta, "ultra").unwrap();
    assert!(text.contains("clamped to high"), "{text}");
    assert_eq!(session.reasoning(), Some(ReasoningLevel::Ultra));
    let shown = model_slash::reasoning(&scope, &mut session, &current, &meta, "").unwrap();
    assert!(shown.contains("high (clamped from ultra)"), "{shown}");
    // Only `--global` writes the level.
    assert!(!config_text(&scope).contains("reasoning_effort"));
    model_slash::reasoning(&scope, &mut session, &current, &meta, "low --global").unwrap();
    model_slash::reasoning(&scope, &mut session, &current, &meta, "show").unwrap();
    model_slash::reasoning(&scope, &mut session, &current, &meta, "full").unwrap();
    let written: toml::Table = toml::from_str(&config_text(&scope)).unwrap();
    assert_eq!(written["agent"]["reasoning_effort"].as_str(), Some("low"));
    assert_eq!(written["display"]["show_reasoning"].as_bool(), Some(true));
    assert_eq!(written["display"]["reasoning_full"].as_bool(), Some(true));
    let err = model_slash::reasoning(&scope, &mut session, &current, &meta, "loud").unwrap_err();
    assert_eq!(err.exit_code(), 2);
}
