//! A local model's window and resources on the daemon's managed server
//! (spec: local-models, Context window and memory placement; Local
//! resource telemetry), with the `pmt-llama-stub` engine: the status
//! bar's context gauge uses the window the server runs with, the session
//! status carries GPU and RAM use, and the session's growth port
//! relaunches the model at a larger window (or says why it cannot).

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use pmt_agent::turn::growth::{Growth, WindowGrowth as _};
use pmt_agent::turn::model::RouteInfo;
use serde_json::{Value, json};

use common::{daemon, ok, run, temp_home};

const GIB: u64 = 1 << 30;

/// A home on `llamacpp/qwen-x` (launched at 65536, native 262144, which
/// the server's model listing reports) with a 24 GiB GPU fixture.
fn local_home() -> tempfile::TempDir {
    let home = temp_home();
    let h = home.path();
    ok(&run(h, &["config", "set", "model.provider", "llamacpp"]));
    ok(&run(h, &["config", "set", "model.default", "qwen-x"]));
    // The stub reports 12 prompt tokens whatever it is sent: a low trigger
    // keeps the measured conversation clearly past it.
    common::append_config(h, "\n[compression]\nthreshold = 0.5\n");
    let version = pmt_local::engine::EngineIndex::embedded().version;
    let engine = h.join(format!("runtimes/llamacpp/{version}-cpu"));
    std::fs::create_dir_all(&engine).unwrap();
    let record = json!({
        "version": version, "backend": "cpu", "platform": "test",
        "exe": pmt_local::testkit::stub(), "library_dirs": [], "archives": [], "installed_at": "",
    });
    std::fs::write(
        engine.join(pmt_local::engine::RECORD_FILE),
        record.to_string(),
    )
    .unwrap();
    let model = h.join("models/qwen-x");
    std::fs::create_dir_all(&model).unwrap();
    std::fs::write(
        model.join("w.gguf"),
        pmt_local::gguf::fixture("qwen3", 262_144, 4, 2, 64),
    )
    .unwrap();
    let record = json!({
        "id": "qwen-x", "source": "catalog", "context": 65536, "added_at": "",
        "files": [{"path": model.join("w.gguf"), "role": "weights", "size": 4}],
    });
    std::fs::write(model.join("model.json"), record.to_string()).unwrap();
    let fixtures = h.join("fixtures");
    std::fs::create_dir_all(&fixtures).unwrap();
    let hardware = json!({"platform": "linux-x64", "ram_total": 32 * GIB, "ram_available": 16 * GIB,
        "gpus": [{"vendor": "nvidia", "name": "NVIDIA GeForce RTX 4090", "memory_total": 24 * GIB,
            "memory_used": 2 * GIB, "cuda": 13, "utilization_percent": 37.0}]});
    std::fs::write(fixtures.join("hardware.json"), hardware.to_string()).unwrap();
    std::fs::write(h.join("daemon.toml"), "[local_runtime]\nenabled = true\n").unwrap();
    home
}

fn rpc(home: &Path, method: &str, params: &Value) -> Value {
    serde_json::from_str(&ok(&run(
        home,
        &["rpc", "call", method, &params.to_string()],
    )))
    .unwrap()
}

/// Submits `text` and waits for its answer.
fn turn(home: &Path, session: &str, text: &str) {
    let before = transcript_len(home, session);
    rpc(
        home,
        "prompt.submit",
        &json!({"session_id": session, "text": text}),
    );
    let deadline = Instant::now() + Duration::from_mins(1);
    while transcript_len(home, session) < before + 2 {
        assert!(Instant::now() < deadline, "the turn never answered");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn transcript_len(home: &Path, session: &str) -> usize {
    rpc(home, "session.transcript", &json!({"session_id": session}))["messages"]
        .as_array()
        .map_or(0, Vec::len)
}

fn usage(home: &Path, session: &str) -> Value {
    rpc(
        home,
        "session.context_usage",
        &json!({"session_id": session}),
    )
}

fn stub_log(home: &Path) -> String {
    std::fs::read_to_string(home.join("run/pmt-llama-stub.log")).unwrap_or_default()
}

/// Scenarios: Gauge shows real window; Window grows before compression
/// (the relaunch); Growth impossible triggers compression (the answer).
#[test]
fn the_gauge_uses_the_served_window_and_the_managed_model_grows() {
    let home = local_home();
    let h = home.path();
    let fixtures = h.join("fixtures");
    let _daemon = daemon::start_env(
        h,
        &[("PMT_TEST_LOCAL_FIXTURES", fixtures.to_str().unwrap())],
    );
    let session = rpc(h, "session.create", &json!({}))["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    turn(h, &session, "hello");
    let first = usage(h, &session);
    // The listing says 262144 (the training window); the server runs 65536.
    assert_eq!(first["context_window"], 65_536, "{first}");
    let resources = &first["local_resources"];
    assert_eq!(resources["gpu_memory_total"], 24 * GIB, "{first}");
    assert_eq!(resources["gpu_utilization_percent"], 37.0);
    assert_eq!(resources["ram_used"], 16 * GIB);

    // The turn loop asks its growth port before compressing (proved with
    // a fake port in pmt-agent's growth tests); here the port itself
    // relaunches the managed server's model at a larger window.
    let catalog = std::sync::Arc::new(pmt_providers::metadata::Catalog::new(
        &pmt_config::Config::default(),
        None,
    ));
    let growth =
        prometheus::local_cmd::growth::LocalGrowth::new(h, std::sync::Arc::clone(&catalog));
    let route = RouteInfo {
        provider: "llamacpp".to_owned(),
        model: "qwen-x".to_owned(),
        price: None,
        context_window: Some(65_536),
        max_output_tokens: None,
        wire_format: pmt_config::schema::WireFormat::ChatCompletions,
        billing_source: "local".to_owned(),
        server_compaction: false,
        local: true,
        fast_tier: false,
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let grown = runtime.block_on(growth.grow(&session, &route, 70_000));
    assert_eq!(
        grown,
        Growth::Grown {
            from: 65_536,
            to: 131_072
        }
    );
    let record = std::fs::read_to_string(h.join("models/qwen-x/model.json")).unwrap();
    let record: Value = serde_json::from_str(&record).unwrap();
    assert_eq!(record["context"], 131_072);
    let log = stub_log(h);
    assert!(
        log.contains("reload") && log.contains("unload qwen-x"),
        "{log}"
    );
    // The window re-reported to the gauge is the one the server reloads.
    let (meta, _) = catalog.metadata_at("llamacpp", "qwen-x", None);
    assert_eq!(meta.context_window, 131_072);
    let next = rpc(h, "session.create", &json!({}))["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    turn(h, &next, "again");
    assert!(
        stub_log(h).contains("load qwen-x c=131072"),
        "{}",
        stub_log(h)
    );
    let after = usage(h, &next);
    assert_eq!(after["context_window"], 131_072, "{after}\n{}", stub_log(h));

    // At the native maximum the window cannot grow: compression runs.
    let route = RouteInfo {
        context_window: Some(262_144),
        ..route
    };
    let mut record = record;
    record["context"] = 262_144.into();
    std::fs::write(h.join("models/qwen-x/model.json"), record.to_string()).unwrap();
    let blocked = runtime.block_on(growth.grow(&session, &route, 300_000));
    assert_eq!(blocked, Growth::Impossible("native_max".to_owned()));
}
