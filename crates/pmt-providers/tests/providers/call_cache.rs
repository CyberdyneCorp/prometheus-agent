//! The canonical request hash and the content-addressed model-call cache
//! (spec: replay-and-evaluation, Canonical request hash; Content-addressed
//! model-call cache; Cache administration and limits).

use std::sync::Arc;
use std::time::Duration;

use pmt_config::schema::{CacheMode, ModelCallCacheConfig, PromptCacheConfig, PromptCacheTtl};
use pmt_obs::usage::UsageRecorder;
use pmt_providers::StreamEvent;
use pmt_providers::calls::{CallKey, Calls, ModelCallCache, Recorded, cache};
use pmt_providers::cost::CallContext;
use pmt_providers::request::{CacheControl, Message, Role};
use pmt_store::{NewSession, Source};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::caching::store;
use crate::support::{Env, caller, request, sse};

const SESSION: &str = "ses_01JZ0000000000000000000000";

fn config(uri: &str) -> String {
    format!(
        "[providers.resp]\nbase_url = \"{uri}/resp/v1\"\nwire_format = \"responses\"\nkey_secret = \"RESP_KEY\"\n\
         [models.\"resp/m\".price]\ninput = 3.0\noutput = 15.0\n"
    )
}

const KEYS: &[(&str, &str)] = &[("RESP_KEY", "resp-key-0123456789")];

fn cache_config(mode: CacheMode) -> ModelCallCacheConfig {
    ModelCallCacheConfig {
        mode,
        ..ModelCallCacheConfig::default()
    }
}

#[tokio::test]
async fn cache_markers_do_not_change_the_hash() {
    let env = Env::new(&config("http://127.0.0.1:9"), KEYS);
    let route = env.route("resp", "m").await;
    let plain = request("hello");
    let mut marked = plain.clone();
    for block in &mut marked.system {
        block.cache_breakpoint = !block.cache_breakpoint;
    }
    marked.messages.push(Message::text(Role::Assistant, "hi"));
    marked.messages[1].cache_breakpoint = true;
    marked.cache = CacheControl::for_session(
        &PromptCacheConfig {
            ttl: PromptCacheTtl::OneHour,
        },
        SESSION,
    );
    let mut unmarked = plain.clone();
    unmarked.messages.push(Message::text(Role::Assistant, "hi"));
    let a = CallKey::of(&route, &marked);
    let b = CallKey::of(&route, &unmarked);
    assert_eq!(a.hash, b.hash);
    assert_eq!(a.hash, marked.hash(&route));
    assert!(a.hash.starts_with("sha256:") && a.hash.len() == 7 + 64);
    // The canonical form names exactly the request's identity.
    let mut keys: Vec<&str> = a
        .canonical
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "messages",
            "model",
            "provider",
            "response_format",
            "sampling",
            "system",
            "tool_choice",
            "tools",
            "wire_api"
        ]
    );
    assert_eq!(a.canonical["wire_api"], "responses");
    assert_eq!(a.canonical["sampling"]["max_output_tokens"], 1024);
}

/// A session row for ledger rows to attach to.
fn session(store: &pmt_store::Store) -> pmt_core::id::SessionId {
    store
        .create_session(&NewSession::new(Source::Cli))
        .unwrap()
        .id
}

#[tokio::test]
async fn a_repeated_call_is_served_from_the_cache_without_reaching_the_provider() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/resp/v1/responses"))
        .respond_with(sse("responses_tool_call.sse"))
        .expect(1)
        .mount(&server)
        .await;
    let env = Env::new(&config(&server.uri()), KEYS);
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let home = dir.path().join("home");
    let store = store(&home);
    let session = session(&store);
    let catalog = Arc::new(pmt_providers::metadata::Catalog::new(&env.config, None));
    let recorder = Arc::new(UsageRecorder::new(store.clone(), catalog));
    let profile = dir.path().join("profile");
    let cache = ModelCallCache::for_profile(&profile, &cache_config(CacheMode::ReadWrite));
    let calls = Arc::new(Calls {
        cache: cache.map(Arc::new),
        ..Calls::default()
    });
    let caller = caller().with_recorder(recorder).with_calls(calls);
    let route = env.route("resp", "m").await;
    let ctx = CallContext::new(session, "main");
    let req = request("hello");

    let mut first = caller.send(&route, &req, Some(&ctx)).await.unwrap();
    let live = first.collect().await;
    assert!(!first.is_served());
    // The rerun of the same request: zero requests reach the provider.
    let mut again = caller.send(&route, &req, Some(&ctx)).await.unwrap();
    let replayed = again.collect().await;
    assert!(again.is_served());
    assert_eq!(replayed, live, "the stored stream replays event for event");
    server.verify().await;

    let rows = store.usage_events(session).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(!rows[0].cache_hit);
    assert!(rows[1].cache_hit, "the rerun's ledger row is a cache hit");
    assert_eq!(rows[1].cost_usd, Some(0.0));
    assert_eq!(rows[1].request_hash, rows[0].request_hash);

    let stats = cache::stats(&profile).unwrap();
    assert_eq!(stats.total.entries, 1);
    let entry = std::fs::read_dir(profile.join(cache::DIR))
        .unwrap()
        .flatten()
        .find(|e| e.path().is_dir())
        .unwrap();
    let file = std::fs::read_dir(entry.path())
        .unwrap()
        .flatten()
        .next()
        .unwrap();
    let hex = rows[0]
        .request_hash
        .as_deref()
        .unwrap()
        .trim_start_matches("sha256:");
    assert_eq!(
        file.file_name().to_string_lossy(),
        format!("{hex}.json.zst")
    );
    assert_eq!(entry.file_name().to_string_lossy(), &hex[..2]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = file.metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    let bytes = std::fs::read(file.path()).unwrap();
    let text = String::from_utf8(zstd::decode_all(bytes.as_slice()).unwrap()).unwrap();
    assert!(
        !text.contains("resp-key-0123456789"),
        "entries hold no credentials"
    );
}

#[tokio::test]
async fn errors_are_not_cached() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/resp/v1/responses"))
        .respond_with(ResponseTemplate::new(529).set_body_string("overloaded"))
        .mount(&server)
        .await;
    let env = Env::new(&config(&server.uri()), KEYS);
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let profile = dir.path().join("profile");
    let cache = ModelCallCache::for_profile(&profile, &cache_config(CacheMode::ReadWrite));
    let calls = Arc::new(Calls {
        cache: cache.map(Arc::new),
        ..Calls::default()
    });
    let caller = caller().with_calls(calls).with_attempt_cap(1);
    let route = env.route("resp", "m").await;
    let failed = caller.send(&route, &request("hello"), None).await;
    assert_eq!(failed.unwrap_err().status, Some(529));
    assert_eq!(cache::stats(&profile).unwrap().total.entries, 0);
}

fn recorded(text: &str) -> Recorded {
    Recorded {
        chunks: vec![
            vec![StreamEvent::TextDelta { text: text.into() }],
            vec![
                StreamEvent::Usage {
                    usage: pmt_providers::Usage::default(),
                },
                StreamEvent::Stop {
                    stop_reason: pmt_providers::StopReason::EndTurn,
                },
            ],
        ],
        usage: pmt_providers::Usage::default(),
        failure: None,
    }
}

fn hash(n: u8) -> String {
    format!("sha256:{}", format!("{n:02x}").repeat(32))
}

#[test]
fn read_only_and_write_only_modes_do_what_they_say() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let write = ModelCallCache::for_profile(dir.path(), &cache_config(CacheMode::Write)).unwrap();
    write.put(&hash(1), ("p", "m"), &recorded("a")).unwrap();
    assert_eq!(write.get(&hash(1)), None, "write-only never serves");
    let read = ModelCallCache::for_profile(dir.path(), &cache_config(CacheMode::Read)).unwrap();
    assert_eq!(read.get(&hash(1)), Some(recorded("a")));
    read.put(&hash(2), ("p", "m"), &recorded("b")).unwrap();
    assert_eq!(cache::stats(dir.path()).unwrap().total.entries, 1);
    assert!(ModelCallCache::for_profile(dir.path(), &cache_config(CacheMode::Off)).is_none());
}

#[test]
fn the_least_recently_used_entries_are_evicted_above_the_limit() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mut config = cache_config(CacheMode::ReadWrite);
    let probe = ModelCallCache::for_profile(dir.path(), &config).unwrap();
    probe.put(&hash(1), ("p", "m"), &recorded("one")).unwrap();
    let one = cache::stats(dir.path()).unwrap().total.bytes;
    // Room for two entries of about this size.
    config.max_bytes = one * 2 + one / 2;
    let cache = ModelCallCache::for_profile(dir.path(), &config).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    cache.put(&hash(2), ("p", "m"), &recorded("two")).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    assert!(cache.get(&hash(1)).is_some(), "a hit makes 1 the newest");
    std::thread::sleep(Duration::from_millis(20));
    cache.put(&hash(3), ("p", "m"), &recorded("thr")).unwrap();
    assert!(cache.get(&hash(2)).is_none(), "2 was used least recently");
    assert!(cache.get(&hash(1)).is_some());
    assert!(cache.get(&hash(3)).is_some());
}

#[test]
fn namespaces_are_kept_apart_and_administered_separately() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let default =
        ModelCallCache::for_profile(dir.path(), &cache_config(CacheMode::ReadWrite)).unwrap();
    let mut eval = cache_config(CacheMode::ReadWrite);
    eval.namespace = "eval".to_owned();
    let eval = ModelCallCache::for_profile(dir.path(), &eval).unwrap();
    default.put(&hash(1), ("p", "m"), &recorded("a")).unwrap();
    eval.put(&hash(2), ("p", "m"), &recorded("b")).unwrap();
    assert!(
        eval.get(&hash(1)).is_none(),
        "namespaces never serve each other"
    );
    let stats = cache::stats(dir.path()).unwrap();
    assert_eq!(stats.total.entries, 2);
    assert_eq!(stats.namespaces["default"].entries, 1);
    assert_eq!(stats.namespaces["eval"].entries, 1);
    // Recently used entries survive an age-limited prune.
    let pruned = cache::prune(dir.path(), Some(Duration::from_hours(24 * 30)), None).unwrap();
    assert_eq!(pruned.entries, 0);
    let cleared = cache::prune(dir.path(), None, Some("eval")).unwrap();
    assert_eq!(cleared.entries, 1);
    assert!(default.get(&hash(1)).is_some());
    assert_eq!(cache::prune(dir.path(), None, None).unwrap().entries, 1);
    assert_eq!(cache::stats(dir.path()).unwrap().total.entries, 0);
}
