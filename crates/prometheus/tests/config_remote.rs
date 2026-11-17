//! The remote configuration backend against the built binary (spec:
//! configuration-and-profiles, Remote configuration backend): a wiremock
//! plane serves layered configuration, writes carry only the changed keys
//! at the profile level, locks name their level, and an unreachable plane
//! fails closed without reading `config.toml`.

use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_prometheus");
const DOC: &str = "/v1/instances/inst-1/profiles/default/config";

fn temp_home() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

fn run(home: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(BIN);
    for var in ["PROMETHEUS_HOME", "PMT_LOG", "RUST_LOG"] {
        cmd.env_remove(var);
    }
    cmd.arg("--home").arg(home).args(args).output().unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A home whose profiles come from `url`, with the plane token stored and
/// a `config.toml` that would not even parse.
fn remote_home(url: &str) -> tempfile::TempDir {
    let home = temp_home();
    let profile = home.path().join("profiles/default");
    std::fs::create_dir_all(&profile).unwrap();
    pmt_core::private_fs::write_atomic(
        &profile.join("secrets.toml"),
        b"CONFIG_PLANE_TOKEN = \"plane-token\"\n",
    )
    .unwrap();
    std::fs::write(profile.join("config.toml"), "this is [not toml").unwrap();
    std::fs::write(
        home.path().join("daemon.toml"),
        format!("[config_backend]\nkind = \"remote\"\nurl = \"{url}\"\ninstance_id = \"inst-1\"\n"),
    )
    .unwrap();
    home
}

fn document(version: &str, model: &str) -> serde_json::Value {
    json!({
        "version": version,
        "layers": [
            {"level": "tenant", "values": {"approvals": {"mode": "manual"}}, "locked": ["approvals.mode"]},
            {"level": "profile", "values": {"model": {"default": model}}}
        ]
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn the_plane_supplies_the_configuration_and_writes_carry_only_changed_keys() {
    let plane = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(DOC))
        .and(header("authorization", "Bearer plane-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(document("1", "remote-model")))
        .mount(&plane)
        .await;
    Mock::given(method("PATCH"))
        .and(path(DOC))
        .and(header("if-match", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"version": "2"})))
        .mount(&plane)
        .await;
    let home = remote_home(&plane.uri());
    let home_path = home.path().to_owned();
    let got =
        pmt_core::spawn::blocking(move || run(&home_path, &["config", "get", "model.default"]))
            .await
            .unwrap();
    assert!(got.status.success(), "{}", stderr(&got));
    assert_eq!(String::from_utf8_lossy(&got.stdout).trim(), "remote-model");

    let home_path = home.path().to_owned();
    let set = pmt_core::spawn::blocking(move || {
        run(&home_path, &["config", "set", "model.default", "mine"])
    })
    .await
    .unwrap();
    assert!(set.status.success(), "{}", stderr(&set));
    let patches: Vec<serde_json::Value> = plane
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "PATCH")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(
        patches,
        [json!({"set": {"model.default": "mine"}, "unset": []})]
    );
    // Nothing was written locally: the bad file is untouched.
    let local = std::fs::read_to_string(home.path().join("profiles/default/config.toml")).unwrap();
    assert_eq!(local, "this is [not toml");

    let home_path = home.path().to_owned();
    let locked = pmt_core::spawn::blocking(move || {
        run(&home_path, &["config", "set", "approvals.mode", "off"])
    })
    .await
    .unwrap();
    assert!(!locked.status.success());
    assert!(
        stderr(&locked).contains("locked by the tenant level"),
        "{}",
        stderr(&locked)
    );
}

#[test]
fn unreachable_plane_fails_closed() {
    // A port nothing listens on.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = format!("http://127.0.0.1:{port}");
    let home = remote_home(&url);
    let start = Instant::now();
    let out = run(home.path(), &["chat", "-z", "hello", "--provider", "mock"]);
    let took = start.elapsed();
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains(&url), "{err}");
    assert!(err.contains("unreachable"), "{err}");
    // The local file would have failed to parse with another error.
    assert!(!err.contains("not toml"), "{err}");
    assert!(took >= Duration::from_secs(25), "gave up after {took:?}");
    assert!(took < Duration::from_mins(1), "took {took:?}");
}
