//! Local models under the gateway daemon (spec: local-models, Managed
//! server lifecycle; Local models command): `prometheus local start`
//! hands the server to the daemon, which supervises it, and the `local.*`
//! methods serve the same documents as the command line.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use common::{daemon, ok, stderr, stdout, temp_home};

/// A home on `llamacpp/qwen-x` with the stub as the installed engine.
fn local_home() -> tempfile::TempDir {
    let home = temp_home();
    let h = home.path();
    ok(&common::run(
        h,
        &["config", "set", "model.provider", "llamacpp"],
    ));
    ok(&common::run(
        h,
        &["config", "set", "model.default", "qwen-x"],
    ));
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
        pmt_local::gguf::fixture("qwen3", 65_536, 4, 2, 64),
    )
    .unwrap();
    let record = json!({
        "id": "qwen-x", "source": "catalog", "context": 65536, "added_at": "",
        "files": [{"path": model.join("w.gguf"), "role": "weights", "size": 4}],
    });
    std::fs::write(model.join("model.json"), record.to_string()).unwrap();
    home
}

fn stub_starts(home: &Path) -> usize {
    std::fs::read_to_string(home.join("run/pmt-llama-stub.log"))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("start "))
        .count()
}

#[test]
fn local_start_hands_the_server_to_the_daemon_and_local_methods_serve_it() {
    let home = local_home();
    let h = home.path();
    let guard = daemon::start(h, &[]);
    let started = common::run(h, &["local", "start"]);
    assert_eq!(started.status.code(), Some(0), "{}", stderr(&started));
    assert!(
        stdout(&started).contains("supervised by the daemon"),
        "{}",
        stdout(&started)
    );
    let record = pmt_local::server::recorded(h).expect("the server runs after the CLI exited");

    let out = common::run(h, &["chat", "-z", "hello"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("stub answer from qwen-x"),
        "{}",
        stdout(&out)
    );
    assert_eq!(stub_starts(h), 1);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (client, _inbox) = daemon::connect(h, "default").await;
        let status = client.call("local.status", json!({})).await.unwrap();
        assert_eq!(status["data"]["status"]["server"]["source"], "managed");
        assert_eq!(status["data"]["status"]["server"]["pid"], json!(record.pid));
        assert_eq!(status["data"]["models"][0]["id"], "qwen-x");
        let catalog = client.call("local.catalog", json!({})).await.unwrap();
        assert!(
            !catalog["data"]["models"].as_array().unwrap().is_empty(),
            "{catalog}"
        );
        let jobs = client.call("local.jobs", json!({})).await.unwrap();
        assert_eq!(jobs["data"]["jobs"], json!([]));
        let missing = client
            .call("local.pause", json!({"job": "nope"}))
            .await
            .unwrap_err();
        assert_eq!(missing.data.unwrap().code, "local.not_found");
        // `..` would name the whole home: refused, nothing deleted.
        let escaped = client
            .call("local.rm", json!({"model": ".."}))
            .await
            .unwrap_err();
        assert_eq!(escaped.data.unwrap().code, "local.not_found");
        assert!(h.join("models/qwen-x/model.json").is_file());
        assert!(
            h.join("profiles/default/config.toml").is_file(),
            "the home survives"
        );
        let used = client
            .call("local.use", json!({"model": "qwen-x"}))
            .await
            .unwrap();
        assert!(
            used["message"]
                .as_str()
                .unwrap()
                .contains("llamacpp/qwen-x"),
            "{used}"
        );
        let stopped: Value = client.call("local.stop", json!({})).await.unwrap();
        assert!(
            stopped["message"].as_str().unwrap().starts_with("stopped"),
            "{stopped}"
        );
        client.close();
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    while record.alive() {
        assert!(Instant::now() < deadline, "the server did not stop");
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(guard);
}

#[test]
fn a_session_hands_the_server_it_needs_to_the_running_daemon() {
    let home = local_home();
    let h = home.path();
    std::fs::write(h.join("daemon.toml"), "[local_runtime]\nenabled = true\n").unwrap();
    let guard = daemon::start(h, &[]);

    // A CLI process resolves the llamacpp route first (validating the
    // model starts the server) while the daemon runs.
    let set = common::run(h, &["model", "set", "llamacpp/qwen-x"]);
    assert_eq!(set.status.code(), Some(0), "{}", stderr(&set));
    // The CLI has exited; the daemon still supervises the server.
    let record = pmt_local::server::recorded(h).expect("the server outlives the CLI");
    let out = common::run(h, &["chat", "-z", "hello"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("stub answer from qwen-x"),
        "{}",
        stdout(&out)
    );
    assert_eq!(stub_starts(h), 1, "the session reused the server");
    assert_eq!(pmt_local::server::recorded(h), Some(record.clone()));

    // It stops with its supervisor, the daemon.
    drop(guard);
    let deadline = Instant::now() + Duration::from_secs(30);
    while record.alive() {
        assert!(Instant::now() < deadline, "the server outlived the daemon");
        std::thread::sleep(Duration::from_millis(50));
    }
}
