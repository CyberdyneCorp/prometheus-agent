//! Resolution table tests (spec scenarios of model-providers).

use std::collections::BTreeMap;
use std::sync::Mutex;

use pmt_config::Config;
use pmt_core::secret::SecretString;

use super::*;

/// A probe answering from a fixed set and recording what it was asked.
#[derive(Default)]
struct FakeProbe {
    up: Vec<String>,
    asked: Mutex<Vec<String>>,
}

impl EndpointProbe for FakeProbe {
    fn reachable<'a>(&'a self, base_url: &'a str) -> BoxFuture<'a, bool> {
        self.asked.lock().unwrap().push(base_url.to_owned());
        let up = self.up.iter().any(|u| u == base_url);
        Box::pin(async move { up })
    }
}

struct Pool;

impl CredentialExtras for Pool {
    fn pooled(&self, provider: &str) -> Option<Pooled> {
        (provider == "openrouter").then(|| Pooled {
            index: 2,
            label: None,
            value: SecretString::new("sk-or-pooled-value-123456"),
        })
    }
}

struct Case {
    config: &'static str,
    secrets: &'static [&'static str],
    selection: Selection,
}

impl Case {
    fn new(config: &'static str, secrets: &'static [&'static str]) -> Self {
        Self {
            config,
            secrets,
            selection: Selection::default(),
        }
    }

    fn flag(mut self, provider: &str) -> Self {
        self.selection.flag.provider = Some(provider.to_owned());
        self
    }

    async fn run_with(
        &self,
        extras: &dyn CredentialExtras,
        probe: &FakeProbe,
    ) -> (Result<Route, Error>, Trace) {
        let config: Config = toml::from_str(self.config).unwrap();
        let registry = ProviderRegistry::for_profile(&config, Vec::new()).unwrap();
        let secrets: BTreeMap<String, SecretString> = self
            .secrets
            .iter()
            .map(|n| {
                (
                    (*n).to_owned(),
                    SecretString::new(format!("value-of-{n}-0123456789")),
                )
            })
            .collect();
        let resolver = Resolver {
            registry: &registry,
            config: &config,
            secrets: &secrets,
            extras,
            probe,
            ad_hoc: None,
        };
        let requested = requested(&self.selection, &config.model);
        let mut trace = Trace::default();
        let route = resolver.resolve(&requested, &mut trace).await;
        (route, trace)
    }

    async fn run(&self) -> Result<Route, Error> {
        self.run_with(&NoExtras, &FakeProbe::default()).await.0
    }
}

fn code(result: &Result<Route, Error>) -> &'static str {
    result.as_ref().map_or_else(|e| e.code().code, |_| "ok")
}

#[tokio::test]
async fn resolution_table() {
    let cases: Vec<(Case, &str, Option<&str>)> = vec![
        // Configured provider with its key.
        (
            Case::new(
                "[model]\nprovider = \"anthropic\"\n",
                &["ANTHROPIC_API_KEY"],
            ),
            "ok",
            Some("anthropic"),
        ),
        // Alias on the command line resolves to the canonical id.
        (
            Case::new("", &["OPENROUTER_API_KEY"]).flag("or"),
            "ok",
            Some("openrouter"),
        ),
        // Typo, disabled, no endpoint, no credential.
        (
            Case::new("", &[]).flag("opnrouter"),
            "provider.unknown",
            None,
        ),
        (
            Case::new(
                "[providers.openrouter]\nenabled = false\n",
                &["OPENROUTER_API_KEY"],
            )
            .flag("openrouter"),
            "provider.disabled",
            None,
        ),
        (
            Case::new("", &["OPENROUTER_API_KEY"]).flag("vllm"),
            "provider.missing_base_url",
            None,
        ),
        (
            Case::new("", &["OPENAI_API_KEY"]).flag("openrouter"),
            "provider.missing_credential",
            None,
        ),
        // Auto: auto_order with a stored secret; nothing configured.
        (
            Case::new("", &["OPENROUTER_API_KEY"]),
            "ok",
            Some("openrouter"),
        ),
        (
            Case::new("", &["ANTHROPIC_API_KEY", "OPENROUTER_API_KEY"]),
            "ok",
            Some("anthropic"),
        ),
        (Case::new("", &[]), "provider.not_configured", None),
        // A model for another provider is not inherited by --provider.
        (
            Case::new(
                "[model]\nprovider = \"anthropic\"\ndefault = \"claude-x\"\n",
                &["DEEPSEEK_API_KEY"],
            )
            .flag("deepseek"),
            "ok",
            Some("deepseek"),
        ),
    ];
    for (case, expected_code, provider) in cases {
        let result = case.run().await;
        assert_eq!(code(&result), expected_code, "{}", case.config);
        if let Some(provider) = provider {
            assert_eq!(result.unwrap().provider, provider);
        }
    }
}

#[tokio::test]
async fn nothing_configured_fails_with_the_not_connected_message() {
    let err = Case::new("", &[]).run().await.unwrap_err();
    assert_eq!(err.message(), auto::NOT_CONNECTED);
    assert_eq!(err.exit_code(), 78);
}

#[tokio::test]
async fn model_comes_from_the_compatible_level_or_the_provider_default() {
    let route = Case::new(
        "[model]\nprovider = \"anthropic\"\ndefault = \"claude-x\"\n",
        &["DEEPSEEK_API_KEY"],
    )
    .flag("deepseek")
    .run()
    .await
    .unwrap();
    assert_eq!(route.model, "deepseek-chat");
    let route = Case::new(
        "[model]\nprovider = \"anthropic\"\ndefault = \"claude-x\"\n",
        &["ANTHROPIC_API_KEY"],
    )
    .run()
    .await
    .unwrap();
    assert_eq!(route.model, "claude-x");
}

#[tokio::test]
async fn disabled_provider_stops_before_credential_resolution() {
    let (result, trace) = Case::new("[providers.openrouter]\nenabled = false\n", &[])
        .flag("openrouter")
        .run_with(&NoExtras, &FakeProbe::default())
        .await;
    assert_eq!(code(&result), "provider.disabled");
    assert!(
        !trace
            .0
            .iter()
            .any(|s| s.step == "credential" || s.step == "endpoint")
    );
}

#[tokio::test]
async fn ollama_default_endpoint_unreachable_never_reaches_openrouter() {
    let probe = FakeProbe::default();
    let (result, trace) = Case::new("", &["OPENROUTER_API_KEY"])
        .flag("ollama")
        .run_with(&NoExtras, &probe)
        .await;
    let err = result.unwrap_err();
    assert_eq!(err.code().code, "provider.missing_base_url");
    assert!(
        err.message().contains("http://127.0.0.1:11434/v1"),
        "{}",
        err.message()
    );
    assert_eq!(*probe.asked.lock().unwrap(), ["http://127.0.0.1:11434/v1"]);
    assert!(!format!("{trace:?}").contains("openrouter.ai"));
    // Reachable default: keyless local route.
    let probe = FakeProbe {
        up: vec!["http://127.0.0.1:11434/v1".into()],
        ..FakeProbe::default()
    };
    let mut case = Case::new("", &[]).flag("ollama");
    case.selection.flag.model = Some("qwen3".into());
    let route = case.run_with(&NoExtras, &probe).await.0.unwrap();
    assert_eq!(route.credential.describe(), "none");
    assert_eq!(route.billing_source(), "local");
}

#[tokio::test]
async fn auto_picks_a_reachable_configured_local_endpoint() {
    let probe = FakeProbe {
        up: vec!["http://127.0.0.1:8000/v1".into()],
        ..FakeProbe::default()
    };
    let config =
        "[model]\ndefault = \"m\"\n[providers.vllm]\nbase_url = \"http://127.0.0.1:8000/v1\"\n";
    let route = Case::new(config, &[])
        .run_with(&NoExtras, &probe)
        .await
        .0
        .unwrap();
    assert_eq!(route.provider, "vllm");
}

#[tokio::test]
async fn lookalike_host_does_not_receive_a_vendor_key() {
    let config = "[providers.mirror]\nbase_url = \"https://api.deepseek.com.attacker.test/v1\"\nkey_secret = \"DEEPSEEK_API_KEY\"\n";
    let mut case = Case::new(config, &["DEEPSEEK_API_KEY"]).flag("mirror");
    case.selection.flag.model = Some("m".into());
    let err = case.run().await.unwrap_err();
    assert_eq!(err.code().code, "provider.missing_credential");
    assert!(
        err.message()
            .contains("DEEPSEEK_API_KEY is bound to api.deepseek.com"),
        "{}",
        err.message()
    );
}

#[tokio::test]
async fn openai_key_never_reaches_an_aggregator() {
    let err = Case::new("", &["OPENAI_API_KEY"])
        .flag("openrouter")
        .run()
        .await
        .unwrap_err();
    assert_eq!(err.code().code, "provider.missing_credential");
    assert_eq!(err.exit_code(), 78);
}

#[tokio::test]
async fn explain_names_the_pool_credential_and_no_key_material() {
    let (route, trace) = Case::new("", &["OPENROUTER_API_KEY"])
        .flag("openrouter")
        .run_with(&Pool, &FakeProbe::default())
        .await;
    assert_eq!(route.unwrap().credential.describe(), "pool:openrouter#2");
    let text = serde_json::to_string(&trace).unwrap();
    assert!(text.contains("pool:openrouter#2"));
    assert!(!text.contains("value-of") && !text.contains("sk-or-pooled"));
}

#[test]
fn precedence_is_flag_then_session_then_config_then_auto() {
    let mut model = pmt_config::schema::ModelConfig {
        provider: Some("anthropic".into()),
        ..Default::default()
    };
    let mut selection = Selection::default();
    assert_eq!(requested(&selection, &model).provider, "anthropic");
    selection.session.provider = Some("deepseek".into());
    assert_eq!(
        requested(&selection, &model).provider_source,
        RequestSource::Session
    );
    selection.flag.provider = Some("xai".into());
    let r = requested(&selection, &model);
    assert_eq!(
        (r.provider.as_str(), r.provider_source),
        ("xai", RequestSource::Flag)
    );
    model.provider = None;
    let r = requested(&Selection::default(), &model);
    assert_eq!(
        (r.provider.as_str(), r.provider_source),
        ("auto", RequestSource::Auto)
    );
}

#[tokio::test]
async fn ad_hoc_key_is_sent_to_its_own_host_only() {
    let config: Config =
        toml::from_str("[[fallback.chain]]\nprovider = \"openrouter\"\nmodel = \"x\"\n").unwrap();
    let registry = ProviderRegistry::for_profile(&config, Vec::new()).unwrap();
    let secrets: BTreeMap<String, SecretString> = BTreeMap::new();
    let probe = FakeProbe::default();
    let key = SecretString::new("sk-adhoc-0123456789abcdef");
    let resolver = Resolver {
        registry: &registry,
        config: &config,
        secrets: &secrets,
        extras: &NoExtras,
        probe: &probe,
        ad_hoc: Some(AdHoc::new("https://llm.example.com/v1", Some(key)).unwrap()),
    };
    // `--base-url` without `--provider` is the custom provider there.
    let selection = Selection {
        flag: Layer {
            provider: None,
            model: Some("m".to_owned()),
        },
        ..Selection::default()
    };
    let requested = requested(&selection, &config.model);
    let route = resolver
        .resolve(&requested, &mut Trace::default())
        .await
        .unwrap();
    assert_eq!(route.provider, "custom");
    assert_eq!(route.base_url, "https://llm.example.com/v1");
    assert_eq!(route.credential.describe(), "flag");
    // A fallback entry on another host never gets the key.
    let fallback = Target {
        provider: "openrouter".to_owned(),
        model: Some("x".to_owned()),
        ..Target::default()
    };
    let err = resolver
        .resolve_target(&fallback, &mut Trace::default())
        .await
        .unwrap_err();
    assert_eq!(err.code().code, "provider.missing_credential");
    // Plain HTTP to a public host is refused as an argument error.
    let err = AdHoc::new("http://llm.example.com/v1", None).unwrap_err();
    assert_eq!(err.exit_code(), 2);
    AdHoc::new("http://127.0.0.1:8080/v1", None).unwrap();
}

#[tokio::test]
async fn plugin_wire_formats_fail_typed_unless_a_native_format_is_chosen() {
    let case = Case::new("", &["GOOGLE_API_KEY"]).flag("gemini");
    let err = case.run().await.unwrap_err();
    assert_eq!(err.code().code, "provider.plugin_unavailable");
    assert!(err.message().contains("gemini_native"));
    // Auto passes over it rather than failing.
    let auto = Case::new(
        "[model]\nauto_order = [\"google\", \"deepseek\"]\n",
        &["GOOGLE_API_KEY", "DEEPSEEK_API_KEY"],
    );
    assert_eq!(auto.run().await.unwrap().provider, "deepseek");
    // Gemini's OpenAI-compatible endpoint is routable natively.
    let openai = Case::new(
        "[providers.gemini]\nbase_url = \"https://generativelanguage.googleapis.com/v1beta/openai\"\n",
        &["GOOGLE_API_KEY"],
    )
    .flag("gemini");
    // It gets past the wire format (gemini has no default model).
    assert_eq!(code(&openai.run().await), "model.not_selected");
    let moa = Case::new("", &[]).flag("moa").run().await.unwrap_err();
    assert_eq!(moa.code().code, "provider.plugin_unavailable");
}
