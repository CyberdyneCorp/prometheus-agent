//! `prometheus local` and `llamacpp` sessions against local fixtures (spec:
//! local-models): fixture hardware and catalogs through
//! `PMT_TEST_LOCAL_FIXTURES`, a slow loopback origin for downloads, and the
//! `pmt-llama-stub` stand-in llama-server as the installed engine.

mod common;

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use serde_json::{Value, json};

use common::{ok, stderr, stdout, temp_home};

const GIB: u64 = 1 << 30;

fn code(out: &Output) -> i32 {
    out.status.code().unwrap()
}

/// A fixtures directory beside the home.
struct Fixtures(PathBuf);

impl Fixtures {
    fn new(home: &Path) -> Self {
        let dir = home.join("fixtures");
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn hardware(&self, platform: &str, gpu_bytes: u64, ram: u64) -> &Self {
        let gpus = if gpu_bytes == 0 {
            json!([])
        } else {
            json!([{"vendor": "nvidia", "name": "NVIDIA GeForce RTX 4060", "memory_total": gpu_bytes, "memory_used": 0, "cuda": 13}])
        };
        let hw =
            json!({"platform": platform, "ram_total": ram, "ram_available": ram, "gpus": gpus});
        std::fs::write(self.0.join("hardware.json"), hw.to_string()).unwrap();
        self
    }

    fn file(&self, name: &str, text: &str) -> &Self {
        std::fs::write(self.0.join(name), text).unwrap();
        self
    }

    fn command(&self, home: &Path) -> Command {
        let mut cmd = common::command(home);
        cmd.env("PMT_TEST_LOCAL_FIXTURES", &self.0);
        cmd
    }

    fn run(&self, home: &Path, args: &[&str]) -> Output {
        self.command(home)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}

fn daemon_toml(home: &Path, text: &str) {
    std::fs::write(home.join("daemon.toml"), text).unwrap();
}

#[test]
fn an_invalid_backend_is_refused_listing_the_accepted_ones_with_exit_78() {
    let home = temp_home();
    daemon_toml(home.path(), "[local_runtime]\nbackend = \"rocm\"\n");
    let out = common::run(home.path(), &["local", "status"]);
    assert_eq!(code(&out), 78, "{}", stderr(&out));
    for backend in ["auto", "cuda", "metal", "vulkan", "hip", "cpu"] {
        assert!(
            stderr(&out).contains(&format!("`{backend}`")),
            "{}",
            stderr(&out)
        );
    }
}

#[test]
fn hip_on_macos_is_refused_with_69_before_any_network_request() {
    let home = temp_home();
    daemon_toml(home.path(), "[local_runtime]\nbackend = \"hip\"\n");
    ok(&common::run(
        home.path(),
        &["config", "set", "model.provider", "mock"],
    ));
    let fixtures = Fixtures::new(home.path());
    fixtures.hardware("darwin-arm64", 0, 16 * GIB);
    // Any request would fail through this dead proxy.
    common::append_config(home.path(), "[network]\nproxy = \"http://127.0.0.1:9\"\n");
    let out = fixtures.run(home.path(), &["local", "install"]);
    assert_eq!(code(&out), 69, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("(hip, darwin-arm64)"),
        "{}",
        stderr(&out)
    );
    assert!(
        !home.path().join("cache/partials").exists(),
        "nothing was planned"
    );
}

#[test]
fn the_catalog_on_an_8_gb_gpu_prices_every_entry_and_offers_nothing_below_4_bits() {
    let home = temp_home();
    let fixtures = Fixtures::new(home.path());
    fixtures.hardware("linux-x64", 8 * GIB, 32 * GIB);
    let out = fixtures.run(home.path(), &["--json", "local", "catalog"]);
    let doc: Value = serde_json::from_str(&ok(&out)).unwrap();
    assert_eq!(doc["schema"], "prometheus.local_catalog.v1");
    assert_eq!(doc["backend"], "cuda");
    let models = doc["models"].as_array().unwrap();
    assert!(!models.is_empty());
    for model in models {
        for key in [
            "fit",
            "context_start",
            "context_max",
            "quant",
            "download_bytes",
        ] {
            assert!(!model[key].is_null(), "{key} missing in {model}");
        }
        assert!(model["bits"].as_f64().unwrap() >= 4.0, "{model}");
    }
    let big = models
        .iter()
        .find(|m| m["id"] == "qwen3.6-35b-a3b")
        .unwrap();
    assert!(
        ["uses_system_ram", "too_big"].contains(&big["fit"].as_str().unwrap()),
        "{big}"
    );
}

#[test]
fn a_linked_file_survives_removing_its_library_entry() {
    let home = temp_home();
    let user = common::user_home(home.path()).join("models");
    std::fs::create_dir_all(&user).unwrap();
    let file = user.join("x.gguf");
    std::fs::write(&file, pmt_local::gguf::fixture("llama", 131_072, 8, 4, 64)).unwrap();
    let added = ok(&common::run(
        home.path(),
        &["local", "add", file.to_str().unwrap()],
    ));
    assert!(added.contains("linked"), "{added}");
    let status: Value = serde_json::from_str(&ok(&common::run(
        home.path(),
        &["--json", "local", "status"],
    )))
    .unwrap();
    assert_eq!(status["models"][0]["id"], "x");
    assert_eq!(status["models"][0]["state"], "ready");
    ok(&common::run(home.path(), &["local", "rm", "x"]));
    assert!(file.is_file(), "the user's file is untouched");
    let status: Value = serde_json::from_str(&ok(&common::run(
        home.path(),
        &["--json", "local", "status"],
    )))
    .unwrap();
    assert_eq!(status["models"], json!([]));
}

/// Serves `size` bytes slowly (64 KiB every 20 ms) to every connection.
fn slow_origin(size: usize) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut buf = [0; 4096];
                let _ = stream.read(&mut buf);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n"
                );
                if stream.write_all(head.as_bytes()).is_err() {
                    return;
                }
                let chunk = vec![7u8; 64 * 1024];
                let mut sent = 0;
                while sent < size {
                    let n = chunk.len().min(size - sent);
                    if stream.write_all(&chunk[..n]).is_err() {
                        return;
                    }
                    sent += n;
                    std::thread::sleep(Duration::from_millis(20));
                }
            });
        }
    });
    port
}

fn catalog_fixture(port: u16, size: u64) -> String {
    json!({
        "schema": "prometheus.local-catalog.v1",
        "hub": format!("http://127.0.0.1:{port}"),
        "models": [{
            "id": "qwen-x", "name": "Qwen X", "repo": "test/qwen-x", "revision": "0".repeat(40),
            "params_b": 1.0, "active_params_b": 1.0, "context_max": 65536,
            "kv_bytes_per_token": 1024, "quality": 1,
            "quants": [{"quant": "Q4_K_M", "bits": 4.85, "files": [
                {"path": "qwen-x-Q4_K_M.gguf", "size": size, "sha256": "0".repeat(64)}
            ]}],
        }],
    })
    .to_string()
}

#[cfg(unix)]
#[test]
fn ctrl_c_pauses_a_pull_and_jobs_lists_it_as_paused() {
    let home = temp_home();
    let size = 64 * 1024 * 1024;
    let port = slow_origin(size);
    let fixtures = Fixtures::new(home.path());
    fixtures
        .hardware("linux-x64", 8 * GIB, 32 * GIB)
        .file("catalog.json", &catalog_fixture(port, size as u64));
    let mut pull = fixtures
        .command(home.path())
        .args(["local", "pull", "qwen-x"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let partials = home.path().join("cache/partials");
    wait_for("download progress", || {
        std::fs::read_dir(&partials)
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| {
                e.path().extension().is_some_and(|x| x == "part")
                    && e.metadata().is_ok_and(|m| m.len() > 0)
            })
    });
    let interrupted = Command::new("kill")
        .args(["-INT", &pull.id().to_string()])
        .status()
        .unwrap();
    assert!(interrupted.success());
    let out = wait_output(&mut pull);
    assert_eq!(code(&out), 130, "{}", stderr(&out));
    assert!(stderr(&out).contains("paused job"), "{}", stderr(&out));

    let jobs: Value =
        serde_json::from_str(&ok(&fixtures.run(home.path(), &["--json", "local", "jobs"])))
            .unwrap();
    let job = &jobs["jobs"][0];
    assert_eq!(job["subject"], "qwen-x");
    assert_eq!(job["state"], "paused", "{jobs}");
    assert!(job["done_bytes"].as_u64().unwrap() < size as u64);
    let status: Value = serde_json::from_str(&ok(
        &fixtures.run(home.path(), &["--json", "local", "status"])
    ))
    .unwrap();
    assert_eq!(
        status["models"][0]["state"], "partial",
        "not usable before verification"
    );
    let text = ok(&fixtures.run(home.path(), &["local", "jobs"]));
    assert!(text.contains("paused"), "{text}");
}

fn wait_output(child: &mut Child) -> Output {
    let deadline = Instant::now() + Duration::from_mins(1);
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the command did not stop");
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut stderr)
        .unwrap();
    Output {
        status: child.wait().unwrap(),
        stdout,
        stderr,
    }
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_mins(1);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A home whose default profile uses `llamacpp/qwen-x`, with the stub as
/// the installed engine and `qwen-x` in the library.
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

fn ledger(home: &Path) -> Vec<pmt_store::UsageEvent> {
    let home = Home::open(home).unwrap();
    let dir = home
        .profile(&ProfileName::parse("default").unwrap())
        .unwrap();
    let scope = ProfileScope::open(dir, &Overrides::default(), Launch::Other).unwrap();
    let store = pmt_store::Store::open(&scope).unwrap();
    let listed = ok(&common::run(
        scope.home(),
        &["--json", "sessions", "list", "--all"],
    ));
    let listed: Value = serde_json::from_str(&listed).unwrap();
    let id = listed["sessions"][0]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    store.usage_events(id).unwrap()
}

#[test]
fn an_offline_turn_on_llamacpp_completes_and_is_billed_local_at_zero() {
    let home = local_home();
    let h = home.path();
    daemon_toml(h, "[local_runtime]\nenabled = true\n");
    // No network: every request but loopback goes to a dead proxy.
    let out = common::command(h)
        .args(["chat", "-z", "hello"])
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stdout(&out).contains("stub answer from qwen-x"),
        "{}",
        stdout(&out)
    );
    let rows = ledger(h);
    let main = rows.iter().find(|r| r.purpose == "main").unwrap();
    assert_eq!(main.billing_source, "local");
    assert_eq!(main.cost_status.as_str(), "exact");
    assert_eq!(main.cost_usd, Some(0.0));
    // The server stops with the process that supervised it.
    wait_for("the managed server to stop", || {
        pmt_local::server::record::live(h).is_none()
    });
}

#[test]
fn a_second_process_reuses_the_recorded_server() {
    let home = local_home();
    let h = home.path();
    daemon_toml(h, "[local_runtime]\nenabled = true\n");
    let mut first = common::command(h)
        .args(["local", "start"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for("the managed server", || {
        pmt_local::server::recorded(h).is_some()
    });
    let out = common::run(h, &["chat", "-z", "hello"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stdout(&out).contains("stub answer from qwen-x"),
        "{}",
        stdout(&out)
    );
    assert_eq!(stub_starts(h), 1, "the second process started no server");
    let status: Value =
        serde_json::from_str(&ok(&common::run(h, &["--json", "local", "status"]))).unwrap();
    assert_eq!(status["status"]["server"]["source"], "managed");
    ok(&common::run(h, &["local", "stop"]));
    let deadline = Instant::now() + Duration::from_secs(30);
    while first.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "local start did not end");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(pmt_local::server::record::read(h).is_none());
}

#[test]
fn a_down_server_fails_with_local_runtime_down_and_never_reaches_openrouter() {
    let home = local_home();
    let h = home.path();
    let origin = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    origin.set_nonblocking(true).unwrap();
    let port = origin.local_addr().unwrap().port();
    common::append_config(
        h,
        &format!("[providers.openrouter]\nbase_url = \"http://127.0.0.1:{port}/api/v1\"\n"),
    );
    common::run_with_stdin(
        h,
        &["secrets", "set", "OPENROUTER_API_KEY"],
        "sk-or-test-0123456789",
    );
    let out = common::run(h, &["--json", "chat", "-z", "hello"]);
    assert_eq!(code(&out), 69, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("provider.local_runtime_down"),
        "{}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("the local model server isn't running"),
        "{}",
        stderr(&out)
    );
    assert!(origin.accept().is_err(), "nothing was sent to OpenRouter");
    assert!(
        pmt_local::server::record::read(h).is_none(),
        "detection only: no server started"
    );
}

/// A `.tar.gz` holding the stub as `llama-test/llama-server`.
fn stub_archive() -> Vec<u8> {
    let stub = std::fs::read(pmt_local::testkit::stub()).unwrap();
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    let mut header = tar::Header::new_gnu();
    header.set_size(stub.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    builder
        .append_data(&mut header, "llama-test/llama-server", stub.as_slice())
        .unwrap();
    builder.into_inner().unwrap().finish().unwrap()
}

/// Scenario path of the `Local models` entry of `prometheus model`: the
/// priced catalog, the engine, the download, then the model as default.
#[cfg(unix)]
#[test]
fn the_model_picker_quickstart_installs_downloads_and_sets_a_local_model() {
    use expectrl::{Eof, Expect as _};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let server = runtime.block_on(MockServer::start());
    let archive = stub_archive();
    let weights = pmt_local::gguf::fixture("qwen3", 65_536, 4, 2, 64);
    runtime.block_on(async {
        Mock::given(method("GET"))
            .and(path("/engine.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(archive.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/test/qwen-x/resolve/{}/qwen-x-Q4_K_M.gguf",
                "0".repeat(40)
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(weights.clone()))
            .mount(&server)
            .await;
    });
    let home = temp_home();
    let h = home.path();
    ok(&common::run(
        h,
        &["config", "set", "model.provider", "mock"],
    ));
    let sha = |bytes: &[u8]| pmt_update::manifest::sha256_hex(bytes);
    let version = pmt_local::engine::EngineIndex::embedded().version;
    let engine = json!({
        "schema": "prometheus.llamacpp-engine.v1", "version": version,
        "builds": {"linux-x64/cpu": {
            "files": [{"url": format!("{}/engine.tar.gz", server.uri()), "size": archive.len(), "sha256": sha(&archive), "format": "tar.gz"}],
            "exe": "llama-test/llama-server",
        }},
    });
    let mut catalog: Value =
        serde_json::from_str(&catalog_fixture(0, weights.len() as u64)).unwrap();
    catalog["hub"] = json!(server.uri());
    catalog["models"][0]["quants"][0]["files"][0]["sha256"] = json!(sha(&weights));
    let fixtures = Fixtures::new(h);
    fixtures
        .hardware("linux-x64", 0, 16 * GIB)
        .file("engine.json", &engine.to_string())
        .file("catalog.json", &catalog.to_string());

    let mut p = common::pty::spawn_with(
        h,
        &["model"],
        &[("PMT_TEST_LOCAL_FIXTURES", fixtures.0.to_str().unwrap())],
    );
    p.expect("provider number").unwrap();
    p.send_line("Local").unwrap();
    p.expect("qwen-x Q4_K_M").unwrap();
    p.expect("model number").unwrap();
    p.send_line("1").unwrap();
    p.expect(Eof).unwrap();
    let status = p.get_process_mut().wait().unwrap();
    assert!(format!("{status:?}").ends_with(", 0)"), "{status:?}");

    let config = std::fs::read_to_string(h.join("profiles/default/config.toml")).unwrap();
    assert!(config.contains("provider = \"llamacpp\""), "{config}");
    assert!(config.contains("default = \"qwen-x\""), "{config}");
    let daemon = pmt_config::daemon::DaemonConfig::load(h).unwrap();
    assert!(daemon.local_runtime.enabled);
    assert!(h.join("models/qwen-x/model.json").is_file());
    assert!(
        h.join(format!(
            "runtimes/llamacpp/{version}-cpu/llama-test/llama-server"
        ))
        .is_file()
    );
}

#[test]
fn an_amd_gpu_on_linux_installs_the_vulkan_build() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let server = runtime.block_on(MockServer::start());
    let archive = stub_archive();
    runtime.block_on(
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(archive.clone()))
            .mount(&server),
    );
    let home = temp_home();
    let h = home.path();
    ok(&common::run(
        h,
        &["config", "set", "model.provider", "mock"],
    ));
    let build = |name: &str| {
        json!({
            "files": [{"url": format!("{}/{name}.tar.gz", server.uri()), "size": archive.len(),
                       "sha256": pmt_update::manifest::sha256_hex(&archive), "format": "tar.gz"}],
            "exe": "llama-test/llama-server",
        })
    };
    let engine = json!({
        "schema": "prometheus.llamacpp-engine.v1", "version": "b1",
        "builds": {"linux-x64/cpu": build("cpu"), "linux-x64/vulkan": build("vulkan"), "linux-x64/hip": build("hip")},
    });
    let amd = json!({"platform": "linux-x64", "ram_total": 32 * GIB, "ram_available": 24 * GIB,
                     "gpus": [{"vendor": "amd", "name": "AMD Radeon RX 7800 XT", "memory_total": 16 * GIB, "memory_used": 0}]});
    let fixtures = Fixtures::new(h);
    fixtures
        .file("hardware.json", &amd.to_string())
        .file("engine.json", &engine.to_string());
    let out = ok(&fixtures.run(h, &["local", "install"]));
    assert!(out.contains("(vulkan)"), "{out}");
    assert!(
        h.join("runtimes/llamacpp/b1-vulkan/llama-test/llama-server")
            .is_file()
    );
    let requested: Vec<String> = runtime
        .block_on(server.received_requests())
        .unwrap()
        .iter()
        .map(|r| r.url.path().to_owned())
        .collect();
    assert_eq!(requested, ["/vulkan.tar.gz"]);
}
