//! Auxiliary calls (spec: model-providers, Auxiliary task routing): aux
//! events only, same-route transient retries, task timeouts and ledger
//! rows under `auxiliary.<task>`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_obs::trajectory::events::Event;
use pmt_obs::usage::UsageRecorder;
use pmt_providers::auxiliary::{
    AuxArgs, AuxCaller, AuxContext, AuxEvents, AuxRoute, AuxSource, AuxTask, resolve,
};
use pmt_providers::cost::CallContext;
use pmt_providers::fallback::Healthy;
use pmt_store::{NewSession, Source};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::caching::store;
use crate::support::{Env, fast, request, sse};

/// Records every event delivered to the session's observer.
#[derive(Default)]
struct Observer(Mutex<Vec<Event>>);

impl AuxEvents for Observer {
    fn emit(&self, event: Event) {
        self.0.lock().unwrap().push(event);
    }
}

fn config(uri: &str) -> String {
    format!(
        "[providers.main]\nbase_url = \"{uri}/main/v1\"\nkey_secret = \"MAIN_KEY\"\n\
         [providers.aux]\nbase_url = \"{uri}/aux/v1\"\nkey_secret = \"AUX_KEY\"\n\
         [auxiliary]\ntransient_retries = 2\n\
         [auxiliary.compression]\nprovider = \"aux\"\nmodel = \"small\"\n"
    )
}

const KEYS: &[(&str, &str)] = &[
    ("MAIN_KEY", "main-key-0123456789"),
    ("AUX_KEY", "aux-key-0123456789"),
];

async fn compression_route(env: &Env) -> AuxRoute {
    let main = env.route("main", "big").await;
    let resolver = env.resolver();
    let ctx = AuxContext {
        resolver: &resolver,
        main: Some(&main),
        health: &Healthy,
    };
    let mut aux = resolve(AuxTask::Compression, &AuxArgs::default(), &ctx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(aux.source, AuxSource::Config);
    fast(&mut aux.route);
    aux
}

#[tokio::test]
async fn aux_call_emits_aux_events_only_retries_and_records_its_purpose() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/aux/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "0"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/aux/v1/chat/completions"))
        .respond_with(sse("chat_text.sse"))
        .mount(&server)
        .await;
    let env = Env::new(&config(&server.uri()), KEYS);
    let aux = compression_route(&env).await;
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let store = store(&dir.path().join("home"));
    let session = store
        .create_session(&NewSession::new(Source::Cli))
        .unwrap()
        .id;
    let catalog = Arc::new(pmt_providers::metadata::Catalog::new(&env.config, None));
    let observer = Arc::new(Observer::default());
    let caller = AuxCaller::new(pmt_net::client(&pmt_net::OutboundPolicy::default()).unwrap())
        .with_recorder(Arc::new(UsageRecorder::new(store.clone(), catalog)))
        .with_events(observer.clone());
    let response = caller
        .call(
            &aux,
            &request("summarise"),
            &CallContext::new(session, "main"),
        )
        .await
        .unwrap();
    assert_eq!(response.attempts, 2);
    assert!(!response.text.is_empty());

    let events = observer.0.lock().unwrap().clone();
    let kinds: Vec<&str> = events.iter().map(Event::kind).collect();
    assert_eq!(
        kinds,
        [
            "aux_call.start",
            "aux_call.end",
            "aux_call.start",
            "aux_call.end"
        ]
    );
    let Event::AuxCallEnd(failed) = &events[1] else {
        unreachable!()
    };
    assert!(!failed.ok);
    let Event::AuxCallStart(second) = &events[2] else {
        unreachable!()
    };
    assert_eq!(second.attempt, 2);
    assert_eq!(second.purpose, "auxiliary.compression");
    assert_eq!(
        (second.provider.as_str(), second.model.as_str()),
        ("aux", "small")
    );
    let Event::AuxCallEnd(done) = &events[3] else {
        unreachable!()
    };
    assert!(done.ok);

    // One ledger row (the failed attempt reported no usage), under the
    // owning session, named like the events.
    let rows = store.usage_events(session).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].purpose, "auxiliary.compression");
    assert_eq!(rows[0].call_id, second.call_id);
    assert_eq!(rows[0].billing_source, "api_key:aux");
    // The main route was never called.
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.url.path().starts_with("/aux/"))
    );
}

#[tokio::test]
async fn retries_are_capped_and_a_task_timeout_ends_an_attempt() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/aux/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "0"))
        .mount(&server)
        .await;
    let env = Env::new(&config(&server.uri()), KEYS);
    let aux = compression_route(&env).await;
    let caller = AuxCaller::new(pmt_net::client(&pmt_net::OutboundPolicy::default()).unwrap());
    let ctx = CallContext::new(pmt_core::id::SessionId::new(), "main");
    let err = caller.call(&aux, &request("x"), &ctx).await.unwrap_err();
    assert_eq!(err.code().code, "provider.call_failed");
    // 1 attempt + transient_retries (2).
    assert_eq!(server.received_requests().await.unwrap().len(), 3);

    let slow = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse("chat_text.sse").set_delay(Duration::from_secs(5)))
        .mount(&slow)
        .await;
    let env = Env::new(&config(&slow.uri()), KEYS);
    let mut aux = compression_route(&env).await;
    aux.timeout = Duration::from_millis(100);
    aux.retries = 0;
    let started = std::time::Instant::now();
    let err = caller.call(&aux, &request("x"), &ctx).await.unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(err.message().contains("compression"), "{}", err.message());
}
