//! Computer-use backends other than the local driver (spec: computer-use,
//! Backend selection and availability; Computer-use CLI and doctor): a
//! backend an enabled `computer-use-backend` plugin provides, and the
//! managed `cua-driver` install through the runtime store.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use common::{append_config, command, mock_home, ok, run, stderr, stdout};
use serde_json::Value;

const SAMPLE: &str = "sample-desktop";

fn plugin_dir(home: &Path, id: &str) -> PathBuf {
    home.join("profiles/default/plugins").join(id)
}

/// Places the sample `computer-use-backend` plugin as `desktop` and enables
/// it with every grant it asks for.
fn install_sample(home: &Path) {
    let dir = plugin_dir(home, "desktop");
    pmt_plugins::testkit::install("sample-computer-use-backend", &dir);
    std::fs::write(
        dir.join("plugin.toml"),
        "manifest_version = 1\nid = \"desktop\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
         description = \"test\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
         worlds = [\"computer-use-backend@1.0\"]\n\n[capabilities]\n",
    )
    .unwrap();
    pmt_plugins::record::InstallRecord {
        source: pmt_plugins::record::Origin::Adopted,
        location: dir.to_string_lossy().into_owned(),
        r#ref: None,
        package_digest: None,
        digest: pmt_plugins::digest::of_dir(&dir).unwrap(),
        signer: None,
        tier: None,
        installed_at: "2026-11-20T00:00:00Z".to_owned(),
        flags: pmt_plugins::record::Flags::default(),
        held_update: None,
    }
    .write(&dir)
    .unwrap();
    ok(&run(
        home,
        &["plugins", "enable", "desktop", "--grant", "all"],
    ));
}

fn plugin_home() -> common::TestHome {
    let home = mock_home();
    install_sample(home.path());
    append_config(
        home.path(),
        &format!(
            "\n[approvals]\nsingle_query_mode = \"approve\"\n[computer_use]\nbackend = \"{SAMPLE}\"\n"
        ),
    );
    home
}

/// The `computer_use` row of `tools list`.
fn tool_row(home: &Path) -> Value {
    let doc: Value = serde_json::from_str(&ok(&run(home, &["--json", "tools", "list"]))).unwrap();
    doc["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "computer_use")
        .unwrap()
        .clone()
}

const DRIVE: &str = r#"
[[rule]]
user = "drive"
[[rule.tool_calls]]
name = "computer_use"
arguments = { action = "capture", mode = "som" }

[[rule]]
step = 1
[[rule.tool_calls]]
name = "computer_use"
arguments = { action = "click", element = 2 }

[[rule]]
repeat = true
text = "done"
"#;

fn drive(home: &Path) -> Output {
    let file = home.join("script.toml");
    std::fs::write(&file, DRIVE).unwrap();
    command(home)
        .current_dir(home)
        .args(["--json", "-z", "drive", "--toolsets", "computer_use"])
        .arg("--mock-script")
        .arg(&file)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// The backend a plugin provides validates, reports its readiness and
/// drives the desktop through the same tool contract.
#[test]
fn a_plugin_backend_is_selected_and_drives_its_desktop() {
    let home = plugin_home();
    let h = home.path();
    ok(&run(h, &["config", "validate"]));
    let status: Value =
        serde_json::from_str(&ok(&run(h, &["--json", "computer-use", "status"]))).unwrap();
    assert_eq!(status["backend"], SAMPLE);
    assert_eq!(status["plugin"], "desktop");
    assert_eq!(status["ready"], true, "{status}");
    assert_eq!(status["detail"], "in-memory sample desktop");
    let row = tool_row(h);
    assert!(
        !row["exposure"].as_str().unwrap().starts_with("unavailable"),
        "{row}"
    );

    let out = drive(h);
    let body: Value = serde_json::from_str(stdout(&out).trim())
        .unwrap_or_else(|e| panic!("{e}: stdout {} stderr {}", stdout(&out), stderr(&out)));
    assert_eq!(body["answer"], "done", "{body}");
    let session = common::newest_session(h);
    let results: Vec<String> = common::session_messages(h, &session)
        .iter()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| m["content"].as_str().map(str::to_owned))
        .collect();
    // The sample's window title names the runtime: the standard mode.
    assert!(results[0].contains("standard:"), "{}", results[0]);
    assert!(
        results[0].contains("cache/computer-use/capture-"),
        "{}",
        results[0]
    );
    assert!(results[1].contains("click ok"), "{}", results[1]);
}

/// A name no enabled plugin provides fails validation and leaves the tool
/// unavailable, never falling back to `cua`.
#[test]
fn a_backend_no_enabled_plugin_provides_is_refused() {
    let home = plugin_home();
    let h = home.path();
    ok(&run(h, &["plugins", "disable", "desktop"]));
    let out = run(h, &["config", "validate"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(&format!(
            "unknown computer-use backend '{SAMPLE}' (expected cua)"
        )),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        tool_row(h)["exposure"],
        format!("unavailable:computer-use backend '{SAMPLE}' is not installed or not enabled")
    );
    let out = run(h, &["--json", "computer-use", "status"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let status: Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(status["ready"], false);

    let typo = mock_home();
    install_sample(typo.path());
    append_config(
        typo.path(),
        "\n[computer_use]\nbackend = \"sample-desktp\"\n",
    );
    let out = run(typo.path(), &["config", "validate"]);
    assert_eq!(out.status.code(), Some(78));
    assert!(
        stderr(&out).contains(&format!("(expected cua, {SAMPLE})")),
        "{}",
        stderr(&out)
    );
}

/// The pinned `cua-driver` of this host: its store directory, executable
/// and archive path below a mirror.
struct Pinned {
    dir: String,
    exe: String,
    mirrored: String,
}

fn pinned(home: &Path) -> Option<Pinned> {
    let store = pmt_update::runtimes::Store::open(home);
    let (pin, artifact) = store.pinned("cua-driver").ok()?;
    Some(Pinned {
        dir: pin.dir_name(artifact),
        exe: artifact.exe.clone(),
        mirrored: format!("/cua-driver/{}/{}", pin.version, artifact.file_name()),
    })
}

/// A runtime mirror that serves bytes other than the pinned archive.
struct Mirror {
    server: wiremock::MockServer,
    runtime: tokio::runtime::Runtime,
}

impl Mirror {
    fn new(home: &Path) -> Self {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .respond_with(
                    wiremock::ResponseTemplate::new(200).set_body_bytes(&b"not the driver"[..]),
                )
                .mount(&server),
        );
        std::fs::write(
            home.join("daemon.toml"),
            format!("[runtimes.mirrors]\nruntimes = \"{}\"\n", server.uri()),
        )
        .unwrap();
        Self { server, runtime }
    }

    fn paths(&self) -> Vec<String> {
        self.runtime
            .block_on(self.server.received_requests())
            .unwrap_or_default()
            .iter()
            .map(|r| r.url.path().to_owned())
            .collect()
    }
}

/// `install` downloads this build's pin through the runtime store, which
/// refuses bytes other than the pinned digest; `--upgrade` over an older
/// version says so first.
#[test]
fn install_fetches_the_pinned_driver_and_refuses_other_bytes() {
    let home = mock_home();
    let h = home.path();
    let mirror = Mirror::new(h);
    let Some(pin) = pinned(h) else {
        // No driver for this host (musl Linux and the like).
        let out = run(h, &["computer-use", "install"]);
        assert_eq!(out.status.code(), Some(69), "{}", stderr(&out));
        return;
    };
    common::fake_runtime(
        h,
        "cua-driver",
        "0.20.0-aaaaaaaaaaaa",
        &[("cua-driver", b"old")],
    );
    let out = run(h, &["computer-use", "install", "--upgrade"]);
    assert_ne!(out.status.code(), Some(0));
    let err = stderr(&out);
    assert!(
        err.contains("upgrading cua-driver to this build's pinned version"),
        "{err}"
    );
    assert!(
        err.contains(&format!(
            "sha256 mismatch for {}{}",
            mirror.server.uri(),
            pin.mirrored
        )),
        "{err}"
    );
    assert_eq!(mirror.paths(), std::slice::from_ref(&pin.mirrored));
    assert!(!h.join("runtimes/cua-driver").join(&pin.dir).exists());
}

/// An installed pin is reported without a download; `--upgrade` keeps the
/// older versions for `runtimes gc`.
#[test]
fn install_reports_the_installed_pin_and_upgrade_keeps_older_versions() {
    let home = mock_home();
    let h = home.path();
    let mirror = Mirror::new(h);
    let Some(pin) = pinned(h) else {
        return;
    };
    let dir = common::fake_runtime(h, "cua-driver", &pin.dir, &[(pin.exe.as_str(), b"driver")]);
    let out = run(h, &["computer-use", "install"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(&format!(
            "cua-driver installed: {}",
            dir.join(&pin.exe).display()
        )),
        "{}",
        stderr(&out)
    );
    common::fake_runtime(
        h,
        "cua-driver",
        "0.20.0-aaaaaaaaaaaa",
        &[("cua-driver", b"old")],
    );
    let out = run(h, &["computer-use", "install", "--upgrade"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("1 older version(s) kept until `prometheus runtimes gc`"),
        "{}",
        stderr(&out)
    );
    assert!(mirror.paths().is_empty(), "nothing downloaded");
}
