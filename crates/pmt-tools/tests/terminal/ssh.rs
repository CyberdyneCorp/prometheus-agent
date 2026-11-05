//! The `ssh` backend against an SSH server container (spec:
//! execution-environments, Backend readiness checks; Remote file sync and
//! sync-back; Infrastructure failures are degraded results; Child
//! environments are built from an allowlist).
//!
//! The server is `sshd/Containerfile` on the container runtime; the tests
//! skip like the container tests when no runtime is reachable.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use pmt_config::schema::{SshConfig, TerminalConfig};
use pmt_core::secret::SecretString;
use pmt_sandbox::backend::Descriptor;
use pmt_sandbox::container::Runtime;
use pmt_sandbox::ssh::SshTarget;
use pmt_tools::files::{FileBackend, RemoteFiles};
use pmt_tools::terminal::remote::ssh::SshTransport;
use pmt_tools::terminal::remote::{RemoteExec, RemoteTarget, SSH_STATE, Transport};
use pmt_tools::terminal::sync::SyncBack;
use pmt_tools::terminal::{Status, Terminal, TerminalArgs};

use crate::common::{Fixture, KNOWN_SECRET};
use crate::container::runtime;

/// A running SSH server and a key that logs in as `pmt`.
struct Server {
    runtime: Runtime,
    id: String,
    config: SshConfig,
    _keys: tempfile::TempDir,
}

impl Server {
    fn start(runtime: Runtime) -> Self {
        let context = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/terminal/sshd");
        let tag = "pmt-test-sshd:1";
        let built = runtime
            .run(&[
                "build".into(),
                "-q".into(),
                "-t".into(),
                tag.into(),
                "-f".into(),
                context.join("Containerfile").to_string_lossy().into_owned(),
                context.to_string_lossy().into_owned(),
            ])
            .unwrap();
        assert!(
            built.status.success(),
            "{}",
            String::from_utf8_lossy(&built.stderr)
        );
        let keys = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let key = keys.path().join("id_ed25519");
        let made = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .status()
            .unwrap();
        assert!(made.success());
        let public = std::fs::read_to_string(key.with_extension("pub")).unwrap();
        let started = runtime
            .run(&[
                "run".into(),
                "-d".into(),
                "--rm".into(),
                "-p".into(),
                "127.0.0.1::22".into(),
                "-e".into(),
                format!("PMT_PUBKEY={}", public.trim()),
                tag.into(),
            ])
            .unwrap();
        assert!(
            started.status.success(),
            "{}",
            String::from_utf8_lossy(&started.stderr)
        );
        let id = String::from_utf8_lossy(&started.stdout).trim().to_owned();
        let port = runtime
            .run(&["port".into(), id.clone(), "22".into()])
            .unwrap();
        let port: u32 = String::from_utf8_lossy(&port.stdout)
            .lines()
            .next()
            .and_then(|l| l.rsplit(':').next())
            .and_then(|p| p.trim().parse().ok())
            .expect("a mapped port");
        let file = keys.path().join("ssh_config");
        std::fs::write(
            &file,
            format!(
                "Host *\n  IdentityFile {}\n  IdentitiesOnly yes\n  StrictHostKeyChecking no\n  UserKnownHostsFile /dev/null\n  LogLevel ERROR\n",
                key.display()
            ),
        )
        .unwrap();
        let config = SshConfig {
            host: Some("127.0.0.1".to_owned()),
            user: Some("pmt".to_owned()),
            port,
            config_file: Some(file.to_string_lossy().into_owned()),
            ..SshConfig::default()
        };
        let server = Self {
            runtime,
            id,
            config,
            _keys: keys,
        };
        server.wait_ready();
        server
    }

    fn transport(&self) -> Arc<dyn Transport> {
        Arc::new(SshTransport::new(
            SshTarget::from_config(&self.config, None).unwrap(),
        ))
    }

    fn wait_ready(&self) {
        let transport = self.transport();
        for _ in 0..60 {
            if transport.connect().is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        panic!("sshd did not come up");
    }

    fn stop(&self) {
        let _ = self
            .runtime
            .run(&["rm".into(), "--force".into(), self.id.clone()]);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Session {
    terminal: Terminal,
    exec: Arc<dyn RemoteExec>,
    fixture: Fixture,
}

fn session(server: &Server, secrets: Vec<(String, SecretString)>) -> Session {
    let fixture = Fixture::new();
    let transport = server.transport();
    let target = Arc::new(RemoteTarget::new(
        Arc::clone(&transport),
        Descriptor::ssh(),
        "ses_ssh",
        SSH_STATE,
    ));
    let env = fixture.child_env(&[], &[]).with_secrets(secrets);
    let mut setup = fixture.setup(TerminalConfig::default(), env, fixture.sandbox(&[], None));
    setup.target = target;
    setup.start_cwd = PathBuf::from("~");
    Session {
        terminal: Terminal::new(setup).unwrap(),
        exec: Arc::new(transport),
        fixture,
    }
}

async fn run(session: &Session, command: &str) -> pmt_tools::terminal::TerminalResult {
    session.terminal.run(TerminalArgs::new(command)).await
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_run_on_the_ssh_host_in_its_home() {
    let Some(runtime) = runtime() else { return };
    let server = Server::start(runtime);
    let session = session(
        &server,
        vec![("PMT_TEST_TOKEN".to_owned(), SecretString::new(KNOWN_SECRET))],
    );
    let who = run(&session, "echo \"$(id -un) $(pwd)\"").await;
    assert_eq!(who.output.trim(), "pmt /home/pmt", "{who:?}");
    // A granted secret reaches the remote command, through a private file
    // that is gone once the command started.
    let secret = run(&session, "echo \"len=${#PMT_TEST_TOKEN}\"; ls ~/.cache/prometheus-terminal | grep -c '^env-' || true").await;
    assert!(
        secret
            .output
            .contains(&format!("len={}", KNOWN_SECRET.len())),
        "{}",
        secret.output
    );
    assert!(secret.output.trim_end().ends_with('0'), "{}", secret.output);
    // Host locations stay behind.
    let env = run(&session, "env").await;
    assert!(
        !env.output
            .contains(&session.fixture.home.to_string_lossy().into_owned())
    );
    // The file tools act on the remote host.
    let files = RemoteFiles::new("ssh", Arc::clone(&session.exec));
    files
        .write(Path::new("/home/pmt/notes/a.txt"), b"remote")
        .unwrap();
    let cat = run(&session, "cat notes/a.txt").await;
    assert_eq!(cat.output.trim(), "remote");
    session.terminal.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ssh_host_unreachable_is_degraded() {
    let Some(runtime) = runtime() else { return };
    let server = Server::start(runtime);
    let session = session(&server, Vec::new());
    assert_eq!(run(&session, "true").await.exit_code, Some(0));
    server.stop();
    let degraded = run(&session, "true").await;
    assert_eq!(degraded.status, Status::Degraded, "{degraded:?}");
    session.terminal.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_back_cap() {
    let Some(runtime) = runtime() else { return };
    let server = Server::start(runtime);
    let session = session(&server, Vec::new());
    let host = session.fixture.work.clone();
    // A 3 KiB budget stands in for the default 2 GiB.
    let sync = SyncBack::new(
        Arc::clone(&session.exec),
        "/home/pmt/ws",
        &host,
        "/home/pmt/.cache/prometheus-terminal/sync-marker",
        3 * 1024,
    );
    run(&session, "mkdir -p ws/out").await;
    sync.start().unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    let made = run(
        &session,
        "head -c 1024 /dev/zero > ws/a.txt && head -c 1024 /dev/zero > ws/out/b.bin && head -c 4096 /dev/zero > ws/out/c.bin && head -c 1024 /dev/zero > ws/out/d.bin",
    )
    .await;
    assert_eq!(made.exit_code, Some(0), "{made:?}");
    let report = sync.sync().unwrap();
    assert_eq!(report.copied, ["a.txt", "out/b.bin", "out/d.bin"]);
    assert_eq!(report.skipped, [("out/c.bin".to_owned(), 4096)]);
    assert!(report.notice(3 * 1024).unwrap().contains("out/c.bin"));
    assert_eq!(
        std::fs::metadata(host.join("out/b.bin")).unwrap().len(),
        1024
    );
    assert!(!host.join("out/c.bin").exists());
    // A host edit after the sync is never overwritten.
    std::thread::sleep(Duration::from_millis(1100));
    std::fs::write(host.join("a.txt"), "host edit").unwrap();
    run(&session, "echo remote > ws/a.txt").await;
    let report = sync.sync().unwrap();
    assert_eq!(report.conflicts, ["a.txt"]);
    assert_eq!(
        std::fs::read_to_string(host.join("a.txt")).unwrap(),
        "host edit"
    );
    session.terminal.close().await;
}
