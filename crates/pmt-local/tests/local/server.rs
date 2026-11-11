//! The llama-server behind `llamacpp` (spec: local-models, Managed server
//! lifecycle; External llama-server detection; Local runtime errors),
//! against the `pmt-llama-stub` stand-in.

use std::path::Path;
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pmt_local::library::{Library, Model, ModelFile, Role, Source};
use pmt_local::server::{self, Runtime, ServerRecord, Stop, record};
use serde_json::{Value, json};

use crate::support::{client, daemon_toml, temp};

/// Installs the stub as the pinned engine's CPU build of `home`.
fn stub_engine(home: &Path) {
    let version = pmt_local::engine::EngineIndex::embedded().version;
    let dir = home.join(format!("runtimes/llamacpp/{version}-cpu"));
    std::fs::create_dir_all(&dir).unwrap();
    let record = json!({
        "version": version, "backend": "cpu", "platform": "test",
        "exe": pmt_local::testkit::stub(), "library_dirs": [],
        "archives": [], "installed_at": "",
    });
    std::fs::write(dir.join(pmt_local::engine::RECORD_FILE), record.to_string()).unwrap();
}

/// A library model `id` (its weights are never read by the stub).
fn stub_model(home: &Path, id: &str) {
    let library = Library::open(home);
    let weights = library.dir(id).unwrap().join("w.gguf");
    std::fs::create_dir_all(library.dir(id).unwrap()).unwrap();
    std::fs::write(&weights, b"GGUF").unwrap();
    library
        .publish(&Model {
            id: id.to_owned(),
            source: Source::Catalog,
            files: vec![ModelFile {
                path: weights,
                role: Role::Weights,
                size: 4,
            }],
            quant: None,
            context: 65_536,
            context_max: None,
            added_at: String::new(),
        })
        .unwrap();
}

/// Supervises the managed server of `home` on a thread, as a client
/// process would; returns once it answers.
fn serve(home: &Path) -> (Stop, std::thread::JoinHandle<()>, ServerRecord) {
    let stop = Stop::default();
    let (tx, rx) = std::sync::mpsc::channel();
    let thread = {
        let stop = stop.clone();
        let home = home.to_path_buf();
        std::thread::spawn(move || {
            server::serve(&home, &stop, &mut |r| {
                let _ = tx.send(r.clone());
            })
            .unwrap();
        })
    };
    let ready = rx
        .recv_timeout(Duration::from_mins(1))
        .expect("the stub starts");
    (stop, thread, ready)
}

fn stub_log(home: &Path) -> Vec<String> {
    std::fs::read_to_string(home.join("run/pmt-llama-stub.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

async fn chat(port: u16, model: &str) -> Value {
    let response = client()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}).to_string())
        .send()
        .await
        .unwrap();
    serde_json::from_slice(&response.bytes().await.unwrap()).unwrap()
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A helper command that must never run.
fn no_helper() -> server::HelperCommand {
    Arc::new(|| Command::new("/nonexistent/prometheus-helper-must-not-run"))
}

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_model_is_unloaded_and_reloaded_by_the_next_request() {
    let home = temp();
    daemon_toml(
        home.path(),
        "[local_runtime]\nenabled = true\nidle_unload_secs = 1\n",
    );
    stub_engine(home.path());
    stub_model(home.path(), "qwen-x");
    let (stop, thread, ready) = serve(home.path());
    assert!(stub_log(home.path())[0].contains("--sleep-idle-seconds 1"));

    let answer = chat(ready.port, "qwen-x").await;
    assert_eq!(
        answer.pointer("/choices/0/message/content"),
        Some(&json!("stub answer from qwen-x"))
    );
    wait_for("the idle unload", || {
        stub_log(home.path()).contains(&"unload qwen-x".to_owned())
    });
    let answer = chat(ready.port, "qwen-x").await;
    assert_eq!(
        answer.pointer("/choices/0/message/content"),
        Some(&json!("stub answer from qwen-x"))
    );
    let loads = stub_log(home.path())
        .iter()
        .filter(|l| *l == "load qwen-x")
        .count();
    assert_eq!(
        loads,
        2,
        "reloaded before answering: {:?}",
        stub_log(home.path())
    );

    stop.raise();
    thread.join().unwrap();
    assert!(
        record::read(home.path()).is_none(),
        "the record goes with the server"
    );
    assert!(!ready.alive(), "the server stops with its supervisor");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_client_reuses_the_recorded_server() {
    let home = temp();
    daemon_toml(home.path(), "[local_runtime]\nenabled = true\n");
    stub_engine(home.path());
    stub_model(home.path(), "qwen-x");
    let (stop, thread, ready) = serve(home.path());
    let endpoint = Runtime::new(home.path(), Some(no_helper()))
        .endpoint()
        .await
        .unwrap();
    assert_eq!(endpoint.source, "managed");
    assert_eq!(
        endpoint.base_url,
        format!("http://127.0.0.1:{}/v1", ready.port)
    );
    let starts = stub_log(home.path())
        .iter()
        .filter(|l| l.starts_with("start "))
        .count();
    assert_eq!(starts, 1);
    stop.raise();
    thread.join().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crashed_server_is_restarted_and_a_stopped_one_is_not() {
    let home = temp();
    daemon_toml(home.path(), "[local_runtime]\nenabled = true\n");
    stub_engine(home.path());
    let (stop, thread, first) = serve(home.path());
    kill(first.pid);
    wait_for("the restart", || {
        record::live(home.path()).is_some_and(|r| r.pid != first.pid)
    });
    let second = record::live(home.path()).unwrap();
    assert_eq!(second.port, first.port);
    let stopped = server::supervise::stop_server(home.path()).unwrap();
    assert_eq!(stopped.pid, second.pid);
    thread.join().unwrap();
    assert!(record::read(home.path()).is_none());
    assert!(
        !stop.raised(),
        "the supervisor ended because the record was removed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_user_run_server_on_a_detect_port_is_preferred_over_starting_one() {
    let home = temp();
    let port = server::launch::free_port().unwrap();
    daemon_toml(
        home.path(),
        &format!("[local_runtime]\nenabled = true\ndetect_ports = [{port}]\n"),
    );
    stub_engine(home.path());
    let _user = UserServer::start(port);
    let endpoint = Runtime::new(home.path(), Some(no_helper()))
        .endpoint()
        .await
        .unwrap();
    assert_eq!(endpoint.source, "external");
    assert_eq!(endpoint.base_url, format!("http://127.0.0.1:{port}/v1"));
    assert!(
        record::read(home.path()).is_none(),
        "no managed server was started"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_default_config_only_detects_and_fails_with_local_runtime_down() {
    let home = temp();
    stub_engine(home.path());
    let err = Runtime::new(home.path(), Some(no_helper()))
        .endpoint()
        .await
        .unwrap_err();
    assert_eq!(err.code().code, "provider.local_runtime_down");
    assert_eq!(err.exit_code(), 69);
    assert!(
        err.message()
            .starts_with("the local model server isn't running"),
        "{}",
        err.message()
    );
    let hint = err.hint().unwrap();
    for remedy in ["prometheus local start", "local_runtime.enabled", "8080"] {
        assert!(hint.contains(remedy), "{hint}");
    }
    assert!(record::read(home.path()).is_none());
}

/// A llama-server the user runs (no preset: a single model).
struct UserServer(Child);

impl UserServer {
    fn start(port: u16) -> Self {
        let child = Command::new(pmt_local::testkit::stub())
            .args(["--port", &port.to_string(), "--alias", "mine"])
            .spawn()
            .unwrap();
        wait_for("the user's server", || server::supervise::health(port));
        Self(child)
    }
}

impl Drop for UserServer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn kill(pid: u32) {
    let mut system = sysinfo::System::new();
    let pid = sysinfo::Pid::from_u32(pid);
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).unwrap().kill();
}
