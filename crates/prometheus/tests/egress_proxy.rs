//! The profile's egress proxy for container and remote sandboxes (spec:
//! security-boundaries, Egress proxy operation and status; Egress proxy
//! coverage of containers and remote sandboxes; Egress credential source
//! and placeholder rotation).
#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;

use common::tls_upstream::upstream;
use common::{append_config, mock_home, ok, run, run_with_stdin, stderr};

const KEY: &str = "sk-real-openai-key-0123456789";

/// A free TCP port.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A profile injecting `OPENAI_API_KEY` toward `host` (empty: the
/// built-in provider hosts), with loopback treated as public.
fn injecting(home: &Path, hosts: &str, extra: &str) {
    append_config(
        home,
        &format!(
            "\n[security.egress]\ncredential_injection = true\nproxy_port = {}\n\
             fake_ip_ranges = [\"127.0.0.0/8\"]\n{extra}\n[security.egress.inject]\nOPENAI_API_KEY = [{hosts}]\n",
            free_port()
        ),
    );
}

fn status(home: &Path) -> serde_json::Value {
    serde_json::from_str(&ok(&run(home, &["--json", "egress", "status"]))).unwrap()
}

#[test]
fn start_status_rotate_reload_and_stop() {
    let home = mock_home();
    injecting(home.path(), "\"localhost\"", "");
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "OPENAI_API_KEY"],
        KEY,
    ));
    assert_eq!(status(home.path())["running"], false);

    ok(&run(home.path(), &["egress", "start"]));
    let record = home.path().join("profiles/default/run/egress-proxy.json");
    let mode = std::fs::metadata(&record).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let shown = status(home.path());
    assert_eq!(shown["running"], true);
    let first = shown["mappings"]["OPENAI_API_KEY"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(first.contains('…'), "{first}");
    let full = ok(&run(home.path(), &["egress", "status", "--show-tokens"]));
    assert!(
        full.contains("pmt-egress-") && !full.contains('…'),
        "{full}"
    );
    assert!(!full.contains(KEY));

    // A second start refuses; rotation needs the confirmation.
    assert_ne!(
        run(home.path(), &["egress", "start"]).status.code(),
        Some(0)
    );
    assert_eq!(
        run(home.path(), &["egress", "rotate"]).status.code(),
        Some(4)
    );
    ok(&run(home.path(), &["egress", "rotate", "--confirm"]));
    let after = status(home.path());
    assert_ne!(after["mappings"]["OPENAI_API_KEY"], first);
    let backups: Vec<_> = std::fs::read_dir(home.path().join("profiles/default/egress"))
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("mappings.json.bak-")
        })
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(
        backups[0].metadata().unwrap().permissions().mode() & 0o777,
        0o600
    );
    ok(&run(home.path(), &["egress", "reload"]));

    ok(&run(home.path(), &["egress", "stop"]));
    assert!(!record.exists());
    assert_eq!(status(home.path())["running"], false);
}

#[test]
fn the_proxy_injects_the_real_key_only_toward_the_mapped_host() {
    let home = mock_home();
    let ca_file = home.path().join("upstream-ca.pem");
    let (port, heads) = upstream(&ca_file);
    injecting(
        home.path(),
        "\"localhost\"",
        &format!("[network]\nextra_ca_file = \"{}\"\n", ca_file.display()),
    );
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "OPENAI_API_KEY"],
        KEY,
    ));
    ok(&run(home.path(), &["egress", "start"]));
    let shown = status(home.path());
    let loopback = shown["loopback"].as_str().unwrap().to_owned();
    let full = ok(&run(
        home.path(),
        &["--json", "egress", "status", "--show-tokens"],
    ));
    let full: serde_json::Value = serde_json::from_str(&full).unwrap();
    let placeholder = full["mappings"]["OPENAI_API_KEY"]
        .as_str()
        .unwrap()
        .to_owned();
    let bundle = home.path().join("bundle.pem");
    let mut pem = std::fs::read(home.path().join("profiles/default/egress/ca.crt")).unwrap();
    pem.extend(std::fs::read(&ca_file).unwrap());
    std::fs::write(&bundle, pem).unwrap();
    let curl = |url: &str| {
        Command::new("curl")
            .args([
                "-sS",
                "--max-time",
                "10",
                "--proxy",
                &format!("http://{loopback}"),
            ])
            .arg("--cacert")
            .arg(&bundle)
            .args(["-H", &format!("Authorization: Bearer {placeholder}"), url])
            .output()
            .unwrap()
    };
    let answer = curl(&format!("https://localhost:{port}/v1/models"));
    assert!(
        answer.status.success(),
        "{}",
        String::from_utf8_lossy(&answer.stderr)
    );
    let seen = heads.lock().unwrap().join("\n");
    assert!(seen.contains(KEY) && !seen.contains(&placeholder), "{seen}");
    // The placeholder toward an unmapped host is refused.
    let elsewhere = curl(&format!("https://127.0.0.1:{port}/steal"));
    let refused = String::from_utf8_lossy(&elsewhere.stdout);
    assert!(refused.contains("credential misdirected"), "{refused}");
    assert!(!heads.lock().unwrap().join("\n").contains("/steal"));
    assert!(status(home.path())["denied"].as_u64().unwrap() >= 1);
    ok(&run(home.path(), &["egress", "stop"]));
}

#[test]
fn bitwarden_missing_a_secret() {
    let home = mock_home();
    let dir = home.path().join("runtimes/bws/9.9.9-test");
    std::fs::create_dir_all(&dir).unwrap();
    let bws = dir.join("bws");
    std::fs::write(
        &bws,
        "#!/bin/sh\necho '[{\"key\":\"ANTHROPIC_API_KEY\",\"value\":\"sk-ant\"}]'\n",
    )
    .unwrap();
    std::fs::set_permissions(&bws, std::fs::Permissions::from_mode(0o755)).unwrap();
    injecting(
        home.path(),
        "",
        "credential_source = \"bitwarden\"\n[secrets.bitwarden]\nproject_id = \"p1\"\n",
    );
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "BWS_ACCESS_TOKEN"],
        "bws-token",
    ));
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "OPENAI_API_KEY"],
        KEY,
    ));
    let out = run(home.path(), &["egress", "start"]);
    assert_eq!(out.status.code(), Some(69), "{}", stderr(&out));
    assert!(stderr(&out).contains("OPENAI_API_KEY"), "{}", stderr(&out));
    assert!(
        !home
            .path()
            .join("profiles/default/run/egress-proxy.json")
            .exists()
    );

    // With the store fallback the stored key is used.
    append_config(home.path(), "");
    ok(&run(
        home.path(),
        &[
            "config",
            "set",
            "security.egress.allow_store_fallback",
            "true",
        ],
    ));
    ok(&run(home.path(), &["egress", "start"]));
    ok(&run(home.path(), &["egress", "stop"]));
}

#[test]
fn proxy_down_with_enforcement() {
    let home = mock_home();
    injecting(home.path(), "", "");
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "OPENAI_API_KEY"],
        KEY,
    ));
    ok(&run(
        home.path(),
        &["config", "set", "terminal.backend", "container"],
    ));
    let script = home.path().join("s.toml");
    std::fs::write(
        &script,
        "[[rule]]\nstep = 0\n[[rule.tool_calls]]\nname = \"terminal\"\narguments = { command = \"echo hi\" }\n\n\
         [[rule]]\nstep = 1\ntext = \"done\"\n",
    )
    .unwrap();
    let out = common::command(home.path())
        .args(["chat", "--yolo", "--mock-script", script.to_str().unwrap()])
        .args(["--json", "-z", "go"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), stderr(&out));
    // The terminal refuses every call with the reason; nothing ran.
    let id = common::newest_session(home.path());
    let sessions = format!("{:?}", common::session_messages(home.path(), &id));
    assert!(
        sessions.contains("egress proxy is not running")
            || text.contains("egress proxy is not running"),
        "{text}\n{sessions}"
    );
}

/// An image with `sh` and `curl`, pinned by digest.
const CURL_IMAGE: &str = "docker.io/curlimages/curl@sha256:c1fe1679c34d9784c1b0d1e5f62ac0a79fca01fb6377cdd33e90473c6f9f9a69";

/// Whether a container runtime answers (else the test skips, unless
/// `PMT_TEST_CONTAINERS=require`).
fn containers() -> bool {
    let found = ["docker", "podman"].iter().any(|cli| {
        Command::new(cli)
            .arg("info")
            .output()
            .is_ok_and(|o| o.status.success())
    });
    assert!(
        found || std::env::var("PMT_TEST_CONTAINERS").as_deref() != Ok("require"),
        "PMT_TEST_CONTAINERS=require but no container runtime answers"
    );
    found
}

/// Scenario: CA inside the container. A container session's command
/// holds only the placeholder, verifies TLS against the mounted bundle
/// and the upstream gets the real key.
#[test]
fn ca_inside_the_container() {
    if !containers() {
        eprintln!("skipping: no container runtime");
        return;
    }
    let home = mock_home();
    let ca_file = home.path().join("upstream-ca.pem");
    let (port, heads) = upstream(&ca_file);
    injecting(
        home.path(),
        "\"localhost\"",
        &format!("[network]\nextra_ca_file = \"{}\"\n", ca_file.display()),
    );
    append_config(
        home.path(),
        &format!(
            "\n[terminal]\nbackend = \"container\"\n\n[terminal.container]\nimage = \"{CURL_IMAGE}\"\nmount_workspace = false\n\n\
             [security.sandbox]\nallow_degraded = [\"all\"]\n"
        ),
    );
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "OPENAI_API_KEY"],
        KEY,
    ));
    ok(&run(home.path(), &["egress", "start"]));
    let command = format!(
        "printf 'held:%s\\n' \"$OPENAI_API_KEY\"; curl -sS --max-time 20 https://localhost:{port}/v1/models -H \"Authorization: Bearer $OPENAI_API_KEY\" && echo VERIFIED"
    );
    let script = home.path().join("s.toml");
    std::fs::write(
        &script,
        format!(
            "[[rule]]\nstep = 0\n[[rule.tool_calls]]\nname = \"terminal\"\narguments = {{ command = '''{command}''' }}\n\n\
             [[rule]]\nstep = 1\ntext = \"done\"\n"
        ),
    )
    .unwrap();
    // The runtime client of this user (Docker Desktop keeps its socket
    // under the real home, which the test's environment does not use).
    let docker_host = Command::new("docker")
        .args([
            "context",
            "inspect",
            "--format",
            "{{.Endpoints.docker.Host}}",
        ])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|h| !h.is_empty());
    let mut chat = common::command(home.path());
    if let Some(host) = docker_host {
        chat.env("DOCKER_HOST", host);
    }
    let out = chat
        .args([
            "chat",
            "--yolo",
            "--mock-script",
            script.to_str().unwrap(),
            "-z",
            "go",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let _ = run(home.path(), &["egress", "stop"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let seen = heads.lock().unwrap().join("\n");
    let id = common::newest_session(home.path());
    let messages = format!("{:?}", common::session_messages(home.path(), &id));
    assert!(seen.contains(KEY), "upstream saw: {seen}\n{messages}");
    assert!(
        messages.contains("VERIFIED") && messages.contains("held:pmt-egress-"),
        "{messages}"
    );
    assert!(!seen.contains("pmt-egress-"), "{seen}");
}
