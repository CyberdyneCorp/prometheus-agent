//! The egress proxy tunnelled to an SSH sandbox (spec: security-boundaries,
//! Egress proxy coverage of containers and remote sandboxes): a reverse
//! forward from the remote host's loopback to the proxy, the CA uploaded,
//! the proxy and CA variables and the placeholders on every command. The
//! SSH server is `fixtures/sshd-egress/Containerfile` on the container
//! runtime; the tests skip without one (unless
//! `PMT_TEST_CONTAINERS=require`).
#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;

use common::tls_upstream::upstream;
use common::{append_config, mock_home, ok, run, run_with_stdin};

const KEY: &str = "sk-real-openai-key-0123456789";
const IMAGE: &str = "pmt-test-sshd-egress:1";

/// Whether a Docker runtime answers.
fn docker() -> bool {
    let found = Command::new("docker")
        .arg("info")
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(
        found || std::env::var("PMT_TEST_CONTAINERS").as_deref() != Ok("require"),
        "PMT_TEST_CONTAINERS=require but no container runtime answers"
    );
    found
}

fn docker_out(args: &[&str]) -> String {
    let out = Command::new("docker").args(args).output().unwrap();
    assert!(
        out.status.success(),
        "docker {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A running SSH server (removed on drop) and the client config that
/// logs in as `pmt`.
struct Server {
    id: String,
    port: String,
    config: std::path::PathBuf,
}

impl Server {
    fn start(dir: &Path, forwarding: bool) -> Self {
        let context = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sshd-egress");
        let file = context.join("Containerfile");
        docker_out(&[
            "build",
            "-q",
            "-t",
            IMAGE,
            "-f",
            file.to_str().unwrap(),
            context.to_str().unwrap(),
        ]);
        let key = dir.join("id_ed25519");
        let made = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .status()
            .unwrap();
        assert!(made.success());
        let public = std::fs::read_to_string(key.with_extension("pub")).unwrap();
        let id = docker_out(&[
            "run",
            "-d",
            "--rm",
            "-p",
            "127.0.0.1::22",
            "-e",
            &format!("PMT_PUBKEY={}", public.trim()),
            "-e",
            &format!("PMT_FORWARDING={}", if forwarding { "yes" } else { "no" }),
            IMAGE,
        ]);
        let port = docker_out(&["port", &id, "22"])
            .lines()
            .next()
            .and_then(|l| l.rsplit(':').next())
            .expect("a mapped port")
            .to_owned();
        let config = dir.join("ssh_config");
        std::fs::write(
            &config,
            format!(
                "Host *\n  IdentityFile {}\n  IdentitiesOnly yes\n  StrictHostKeyChecking no\n  UserKnownHostsFile /dev/null\n  LogLevel ERROR\n",
                key.display()
            ),
        )
        .unwrap();
        let server = Self { id, port, config };
        server.wait_ready();
        server
    }

    fn wait_ready(&self) {
        for _ in 0..100 {
            let login = Command::new("ssh")
                .arg("-F")
                .arg(&self.config)
                .args([
                    "-o",
                    "BatchMode=yes",
                    "-p",
                    &self.port,
                    "pmt@127.0.0.1",
                    "true",
                ])
                .output()
                .unwrap();
            if login.status.success() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        panic!("the SSH server did not accept a login");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.id]).output();
    }
}

/// A profile on the SSH backend injecting `OPENAI_API_KEY` toward
/// `localhost` (loopback treated as public), with the proxy started.
fn ssh_home(server: &Server, ca_file: &Path) -> common::TestHome {
    let home = mock_home();
    let proxy_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    append_config(
        home.path(),
        &format!(
            "\n[network]\nextra_ca_file = \"{}\"\n\n[terminal]\nbackend = \"ssh\"\n\n\
             [terminal.ssh]\nhost = \"127.0.0.1\"\nuser = \"pmt\"\nport = {}\nconfig_file = \"{}\"\n\n\
             [security.egress]\ncredential_injection = true\nproxy_port = {proxy_port}\n\
             fake_ip_ranges = [\"127.0.0.0/8\"]\n\n[security.egress.inject]\nOPENAI_API_KEY = [\"localhost\"]\n",
            ca_file.display(),
            server.port,
            server.config.display()
        ),
    );
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "OPENAI_API_KEY"],
        KEY,
    ));
    ok(&run(home.path(), &["egress", "start"]));
    home
}

/// Runs `command` in a chat turn; the terminal tool's output.
fn terminal_output(home: &Path, command: &str) -> String {
    let script = home.join("s.toml");
    std::fs::write(
        &script,
        format!(
            "[[rule]]\nstep = 0\n[[rule.tool_calls]]\nname = \"terminal\"\narguments = {{ command = '''{command}''' }}\n\n\
             [[rule]]\nstep = 1\ntext = \"done\"\n"
        ),
    )
    .unwrap();
    let out = common::command(home)
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
    let _ = run(home, &["egress", "stop"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let id = common::newest_session(home);
    common::session_messages(home, &id)
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// Scenario (SSH): a command on the SSH host holds only the placeholder,
/// reaches the upstream through the tunnelled proxy, verifies TLS
/// against the uploaded CA, and the upstream gets the real key.
#[test]
fn an_ssh_sandbox_goes_through_the_tunnelled_proxy() {
    if !docker() {
        eprintln!("skipping: no container runtime");
        return;
    }
    let keys = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let server = Server::start(keys.path(), true);
    let ca_file = keys.path().join("upstream-ca.pem");
    let (port, heads) = upstream(&ca_file);
    let home = ssh_home(&server, &ca_file);
    let output = terminal_output(
        home.path(),
        &format!(
            "printf 'held:%s\\n' \"$OPENAI_API_KEY\"; printf 'proxy:%s\\n' \"$HTTPS_PROXY\"; \
             curl -sS --max-time 20 https://localhost:{port}/v1/models -H \"Authorization: Bearer $OPENAI_API_KEY\" && echo VERIFIED"
        ),
    );
    let seen = heads.lock().unwrap().join("\n");
    assert!(seen.contains(KEY), "upstream saw: {seen}\n{output}");
    assert!(!seen.contains("pmt-egress-"), "{seen}");
    assert!(output.contains("held:pmt-egress-"), "{output}");
    assert!(output.contains("proxy:http://127.0.0.1:"), "{output}");
    assert!(output.contains("VERIFIED"), "{output}");
    assert!(!output.contains(KEY), "{output}");
}

/// A server that refuses port forwarding gets no command run: the
/// session fails closed rather than reaching the network around the
/// proxy.
#[test]
fn an_ssh_host_refusing_the_tunnel_runs_nothing() {
    if !docker() {
        eprintln!("skipping: no container runtime");
        return;
    }
    let keys = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let server = Server::start(keys.path(), false);
    let ca_file = keys.path().join("upstream-ca.pem");
    let (port, heads) = upstream(&ca_file);
    let home = ssh_home(&server, &ca_file);
    let output = terminal_output(
        home.path(),
        &format!("echo RAN; curl -sk --max-time 5 https://localhost:{port}/"),
    );
    assert!(!output.contains("RAN"), "{output}");
    assert!(output.contains("cannot forward"), "{output}");
    assert!(heads.lock().unwrap().is_empty());
}
