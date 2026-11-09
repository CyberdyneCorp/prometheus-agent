//! Retained receipts of ended background processes (spec:
//! execution-environments, Retained process receipts): readable from the
//! owning conversation after compression, invisible to any other.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::secret::Redactor;
use pmt_store::{LineageKind, NewSession, Origin, Source, Store};
use pmt_tools::terminal::process::{Job, JobEnd, Options, ProcessArgs, ProcessRegistry, Tracking};
use pmt_tools::terminal::spawn::Group;
use prometheus::chat::session::processes::Wiring;
use serde_json::Value;

use common::temp_home;

fn registry(
    scope: &ProfileScope,
    store: &Arc<Store>,
    session: pmt_core::id::SessionId,
) -> ProcessRegistry {
    let wiring = Wiring::of(scope.config(), store, session, &Origin::None);
    ProcessRegistry::new(Options {
        kill_grace: Duration::from_secs(1),
        max_chars: pmt_tools::setting::fixed(50_000),
        redactor: Redactor::default(),
        notify: None,
        clock: Arc::new(Instant::now),
        auto_notify: false,
        receipts: Some(wiring.receipts),
    })
}

async fn act(registry: &ProcessRegistry, action: &str, id: &str) -> Value {
    registry
        .handle(&ProcessArgs {
            action: action.to_owned(),
            process_id: Some(id.to_owned()),
            data: None,
            timeout: Some(10.0),
        })
        .await
}

/// Scenarios: Read after compression; Other conversation.
#[tokio::test]
async fn a_receipt_is_read_by_its_continuation_and_hidden_from_other_conversations() {
    let home = temp_home();
    let root = Home::open(home.path()).unwrap();
    let dir = root.profile(&ProfileName::default_profile()).unwrap();
    let scope = ProfileScope::open(dir, &Overrides::default(), Launch::Other).unwrap();
    let store = Arc::new(Store::open(&scope).unwrap());
    let first = store
        .create_session(&NewSession::new(Source::Cli))
        .unwrap()
        .id;

    let running = registry(&scope, &store, first);
    let job = Job {
        run: Box::pin(async {
            JobEnd {
                exit_code: 0,
                output: "built 42 crates\n".into(),
            }
        }),
        group: Group::local(None),
    };
    let id = running.track_job(job, &Tracking::new("make build", false));
    assert_eq!(act(&running, "wait", &id).await["state"], "exited");
    drop(running);

    let mut continued = NewSession::new(Source::Cli);
    continued.parent = Some((first, LineageKind::Compression));
    let continuation = store.create_session(&continued).unwrap().id;
    let after = registry(&scope, &store, continuation);
    let log = act(&after, "log", &id).await;
    assert_eq!(log["retained"], true, "{log}");
    assert_eq!(log["exit_code"], 0);
    assert_eq!(log["output"], "built 42 crates\n");
    let listed = after
        .handle(&ProcessArgs {
            action: "list".into(),
            ..ProcessArgs::default()
        })
        .await;
    assert_eq!(listed["processes"][0]["process_id"], id.as_str());
    assert_eq!(listed["processes"][0]["retained"], true);

    let other = store
        .create_session(&NewSession::new(Source::Cli))
        .unwrap()
        .id;
    let unrelated = registry(&scope, &store, other);
    let poll = act(&unrelated, "poll", &id).await;
    assert_eq!(poll["status"], "error");
    assert!(
        poll["error"]
            .as_str()
            .unwrap()
            .contains("unknown process id"),
        "{poll}"
    );
}
