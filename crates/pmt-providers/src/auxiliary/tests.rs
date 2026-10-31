//! Auxiliary routing scenarios (spec: model-providers, Auxiliary task
//! routing; Auxiliary auto route never guesses another account).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_config::Config;
use pmt_core::secret::SecretString;

use super::*;
use crate::fallback::Healthy;
use crate::registry::ProviderRegistry;
use crate::resolve::{CredentialSource, NoExtras, TcpProbe};

static PROBE: TcpProbe = TcpProbe {
    timeout: Duration::from_millis(200),
};

struct Env {
    config: Config,
    registry: ProviderRegistry,
    secrets: BTreeMap<String, SecretString>,
}

impl Env {
    fn new(config: &str, secrets: &[&str]) -> Self {
        let config: Config = toml::from_str(config).unwrap();
        let registry = ProviderRegistry::for_profile(&config, Vec::new()).unwrap();
        let secrets = secrets
            .iter()
            .map(|n| {
                (
                    (*n).to_owned(),
                    SecretString::new(format!("{n}-0123456789")),
                )
            })
            .collect();
        Self {
            config,
            registry,
            secrets,
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

    async fn main_route(&self, provider: &str, model: &str) -> Route {
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

    async fn aux(&self, task: AuxTask, main: Option<&Route>) -> Result<Option<AuxRoute>, Error> {
        let resolver = self.resolver();
        let ctx = AuxContext {
            resolver: &resolver,
            main,
            health: &Healthy,
        };
        resolve(task, &AuxArgs::default(), &ctx).await
    }
}

/// Every provider is benched (its credentials are exhausted).
struct AllBenched;

impl RouteHealth for AllBenched {
    fn benched_for(&self, _provider: &str, _now: jiff::Timestamp) -> Option<Duration> {
        Some(Duration::from_hours(1))
    }
}

#[tokio::test]
async fn per_task_model_pin_routes_to_that_provider_and_model() {
    let env = Env::new(
        "[auxiliary.compression]\nprovider = \"openrouter\"\nmodel = \"google/gemini-3.6-flash\"\n",
        &["OPENROUTER_API_KEY", "ANTHROPIC_API_KEY"],
    );
    let main = env.main_route("anthropic", "claude-sonnet-4-5").await;
    let aux = env
        .aux(AuxTask::Compression, Some(&main))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(aux.route.label(), "openrouter/google/gemini-3.6-flash");
    assert_eq!(aux.source, AuxSource::Config);
    assert_eq!(aux.timeout, Duration::from_mins(2));
}

#[tokio::test]
async fn auto_uses_the_main_route_and_its_credential_only() {
    let env = Env::new("", &["ANTHROPIC_API_KEY", "DEEPSEEK_API_KEY"]);
    let main = env.main_route("anthropic", "claude-sonnet-4-5").await;
    let aux = env.aux(AuxTask::Title, Some(&main)).await.unwrap().unwrap();
    assert_eq!(aux.source, AuxSource::Main);
    assert_eq!(aux.route, main);
    assert_eq!(
        aux.route.credential.source,
        CredentialSource::Secret {
            name: "ANTHROPIC_API_KEY".to_owned()
        }
    );
    assert_eq!(aux.timeout, Duration::from_secs(30));
    assert_eq!(aux.retries, 2);
}

/// Captures WARN records.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn main_provider_down_without_a_chain_never_uses_an_unrelated_key() {
    let env = Env::new(
        "[model]\nprovider = \"anthropic\"\n",
        &[
            "ANTHROPIC_API_KEY",
            "DEEPSEEK_API_KEY",
            "OPENROUTER_API_KEY",
        ],
    );
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    // The main route is unavailable: not resolved at all, or benched.
    assert_eq!(env.aux(AuxTask::Title, None).await.unwrap(), None);
    let main = env.main_route("anthropic", "claude-sonnet-4-5").await;
    let resolver = env.resolver();
    let benched = AuxContext {
        resolver: &resolver,
        main: Some(&main),
        health: &AllBenched,
    };
    let none = resolve(AuxTask::Title, &AuxArgs::default(), &benched).await;
    assert_eq!(none.unwrap(), None);
    let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("WARN"), "{logs}");
    assert!(logs.contains("auxiliary task 'title'"), "{logs}");
}

#[tokio::test]
async fn auto_falls_back_to_the_task_chain_then_the_profile_chain() {
    let env = Env::new(
        "[auxiliary.title]\nfallback_chain = [\"deepseek/deepseek-chat\"]\n\
         [[fallback.chain]]\nprovider = \"openrouter\"\nmodel = \"x\"\n",
        &["DEEPSEEK_API_KEY", "OPENROUTER_API_KEY"],
    );
    let aux = env.aux(AuxTask::Title, None).await.unwrap().unwrap();
    assert_eq!(aux.route.label(), "deepseek/deepseek-chat");
    assert_eq!(aux.source, AuxSource::TaskFallback { index: 0 });
    // The task's entry has no credential: the profile chain answers.
    let env = Env::new(
        "[auxiliary.title]\nfallback_chain = [\"deepseek/deepseek-chat\"]\n\
         [[fallback.chain]]\nprovider = \"openrouter\"\nmodel = \"x\"\n",
        &["OPENROUTER_API_KEY"],
    );
    let aux = env.aux(AuxTask::Title, None).await.unwrap().unwrap();
    assert_eq!(aux.route.label(), "openrouter/x");
    assert_eq!(aux.source, AuxSource::ProfileFallback { index: 0 });
    // `fallback_chain = []` disables fallback for the task.
    let env = Env::new(
        "[auxiliary.title]\nfallback_chain = []\n\
         [[fallback.chain]]\nprovider = \"openrouter\"\nmodel = \"x\"\n",
        &["OPENROUTER_API_KEY"],
    );
    assert_eq!(env.aux(AuxTask::Title, None).await.unwrap(), None);
}

#[tokio::test]
async fn explicit_arguments_beat_config_and_a_model_alone_stays_on_the_main_provider() {
    let env = Env::new(
        "[auxiliary.vision]\nprovider = \"openrouter\"\nmodel = \"v\"\ntimeout_secs = 5\n",
        &["OPENROUTER_API_KEY", "ANTHROPIC_API_KEY"],
    );
    let main = env.main_route("anthropic", "claude-sonnet-4-5").await;
    let resolver = env.resolver();
    let ctx = AuxContext {
        resolver: &resolver,
        main: Some(&main),
        health: &Healthy,
    };
    let args = AuxArgs {
        provider: None,
        model: Some("claude-haiku-4-5".to_owned()),
    };
    let aux = resolve(AuxTask::Vision, &args, &ctx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(aux.route.label(), "anthropic/claude-haiku-4-5");
    assert_eq!(aux.source, AuxSource::Explicit);
    assert_eq!(aux.timeout, Duration::from_secs(5));
    let aux = resolve(AuxTask::Vision, &AuxArgs::default(), &ctx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(aux.route.label(), "openrouter/v");
    // A pinned route that cannot resolve is an error, never `auto`.
    let pinned = AuxArgs {
        provider: Some("deepseek".to_owned()),
        model: Some("deepseek-chat".to_owned()),
    };
    let err = resolve(AuxTask::Vision, &pinned, &ctx).await.unwrap_err();
    assert_eq!(err.code().code, "provider.missing_credential");
}

#[test]
fn task_timeouts_and_purposes() {
    let expected = [
        (AuxTask::Vision, Some(120)),
        (AuxTask::Compression, Some(120)),
        (AuxTask::Approval, Some(30)),
        (AuxTask::Title, Some(30)),
        (AuxTask::Curator, Some(600)),
        (AuxTask::Evolution, Some(600)),
        (AuxTask::SideQuestion, None),
    ];
    for (task, secs) in expected {
        assert_eq!(
            task.default_timeout(),
            secs.map(Duration::from_secs),
            "{task:?}"
        );
    }
    assert_eq!(AuxTask::Title.purpose(), "auxiliary.title");
    assert_eq!(AuxTask::ALL.len(), 12);
    for task in AuxTask::ALL {
        assert_eq!(AuxTask::parse(task.as_str()), Some(*task));
    }
}

#[test]
fn misspelled_task_section_fails_validation_naming_it() {
    let err = pmt_config::check(
        std::path::Path::new("config.toml"),
        "[auxiliary.compresion]\nmodel = \"x\"\n",
    )
    .unwrap_err();
    assert!(
        err.message().contains("auxiliary.compresion"),
        "{}",
        err.message()
    );
    assert_eq!(err.exit_code(), 78);
}
