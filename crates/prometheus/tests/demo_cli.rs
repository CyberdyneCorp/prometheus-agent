//! `prometheus demo` (spec: replay-and-evaluation, Offline smoke demo):
//! the bundled ledger scenario run end to end by the built binary with the
//! mock provider, in a home of its own, without the network.

mod common;

use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::{stdout, temp_home};

/// The spec's bound on the reference runner.
const LIMIT: Duration = Duration::from_secs(30);

/// `prometheus demo ARGS` with the user's home at `<base>/user-home`
/// (`--home`) and at `<base>/env-home` (`PROMETHEUS_HOME`), the temporary
/// directory at `<base>/tmp`, and every proxy variable pointing at
/// `proxy`, so a connection attempt would be counted.
fn demo(base: &Path, proxy: &str, args: &[&str]) -> Output {
    demo_command(base, proxy, args).output().unwrap()
}

fn demo_command(base: &Path, proxy: &str, args: &[&str]) -> Command {
    let tmp = base.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let mut cmd: Command = common::command(&base.join("user-home"));
    cmd.arg("demo")
        .args(args)
        .current_dir(base)
        .env("PROMETHEUS_HOME", base.join("env-home"))
        .env("TMPDIR", &tmp)
        .env("TMP", &tmp)
        .env("TEMP", &tmp)
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .stdin(Stdio::null());
    for var in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
        cmd.env(var, proxy).env(var.to_lowercase(), proxy);
    }
    cmd
}

/// A stand-in for the network: counts and drops every connection.
fn counting_proxy() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&count);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            seen.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });
    (url, count)
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn assert_passed(out: &Output) -> String {
    let text = stdout(out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout:\n{text}\nstderr:\n{}",
        common::stderr(out)
    );
    text
}

/// Scenario: First run on a fresh machine (no providers configured): the
/// demo completes, prints each step in the canonical trajectory rendering,
/// reports `Total: 85.15`, prints `demo passed` and exits 0 within 30 s,
/// with no connection and without touching the user's homes.
#[test]
fn first_run_on_a_fresh_machine() {
    let base = temp_home();
    let (proxy, connections) = counting_proxy();

    let started = Instant::now();
    let out = demo(base.path(), &proxy, &[]);
    let elapsed = started.elapsed();

    let text = assert_passed(&out);
    assert!(elapsed < LIMIT, "took {elapsed:?}");
    // The canonical steps, in order: inspect, run, read, patch, re-run,
    // answer.
    let steps = [
        "[USER]\n",
        "[ACTION] read_file({\"path\":\"README.md\"})",
        "[ACTION] terminal(",
        "Total: 235.35",
        "[ACTION] read_file(",
        "[ACTION] patch(",
        "[ACTION] terminal(",
        "[OBSERVATION]\n{\"exit_code\":0,\"output\":\"Total: 85.15",
        "[ASSISTANT]\nFixed the sign bug",
        "Total: 85.15\n\ndemo passed in ",
    ];
    let mut rest = text.as_str();
    for step in steps {
        let at = rest
            .find(step)
            .unwrap_or_else(|| panic!("missing {step:?} in order:\n{text}"));
        rest = &rest[at + step.len()..];
    }
    assert_eq!(connections.load(Ordering::SeqCst), 0, "the demo connected");
    // The user's homes: `--home` holds only the test's user-home directory,
    // PROMETHEUS_HOME was never created, and the temporary directory is
    // gone.
    assert_eq!(entries(&base.path().join("user-home")), ["user-home"]);
    assert!(!base.path().join("env-home").exists());
    assert!(entries(&base.path().join("tmp")).is_empty());
}

/// `--output DIR --keep` leaves the demo's home (with the session and its
/// trajectory) and the fixed repository in DIR.
#[test]
fn a_kept_run_can_be_inspected() {
    let base = temp_home();
    let (proxy, _) = counting_proxy();

    let out = demo(base.path(), &proxy, &["--output", "run", "--keep"]);

    let text = assert_passed(&out);
    let dir = base.path().join("run");
    assert_eq!(entries(&dir), ["home", "ledger", "mock.toml"]);
    let script = entries(&dir.join("ledger"))
        .into_iter()
        .find(|name| name.starts_with("total."))
        .unwrap();
    let fixed = std::fs::read_to_string(dir.join("ledger").join(&script)).unwrap();
    assert!(fixed.contains("total - cents") || fixed.contains("$total -= $cents"));
    let kept = text.lines().last().unwrap();
    assert!(kept.starts_with("kept "), "{text}");
    let session = kept.rsplit(' ').next().unwrap();
    let shown = common::ok(&common::run(
        &dir.join("home"),
        &["trajectory", "show", session],
    ));
    assert!(shown.contains("Total: 85.15"), "{shown}");
}

/// Without `--keep` the demo removes what it created: a directory it
/// made is deleted, an empty one the user named is emptied, and a
/// non-empty one is refused (exit 2) and left alone.
#[test]
fn the_demo_cleans_up_only_what_it_created() {
    let base = temp_home();
    let (proxy, _) = counting_proxy();

    assert_passed(&demo(base.path(), &proxy, &["--output", "fresh"]));
    assert!(!base.path().join("fresh").exists());

    std::fs::create_dir(base.path().join("empty")).unwrap();
    assert_passed(&demo(base.path(), &proxy, &["--output", "empty"]));
    assert!(entries(&base.path().join("empty")).is_empty());

    let busy = base.path().join("busy");
    std::fs::create_dir(&busy).unwrap();
    std::fs::write(busy.join("notes.txt"), "mine").unwrap();
    let out = demo(base.path(), &proxy, &["--output", "busy"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        common::stderr(&out).contains("is not an empty directory"),
        "{}",
        common::stderr(&out)
    );
    assert_eq!(entries(&busy), ["notes.txt"]);
}

/// A `bash-language-server` on `PATH` that would run if a session started
/// it (Unix: the demo's script is `total.sh`).
#[cfg(unix)]
fn fake_language_server(bin: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(bin).unwrap();
    let server = bin.join("bash-language-server");
    std::fs::write(&server, "#!/bin/sh\nexec sleep 30\n").unwrap();
    std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(not(unix))]
fn fake_language_server(_bin: &Path) {}

/// Even when its repository sits inside a git working tree (a checkout
/// holding the temporary directory), the demo starts no language server
/// for the script it patches, installs none and makes no connection.
#[test]
fn inside_a_git_tree_the_demo_starts_and_installs_no_language_server() {
    let base = temp_home();
    let (proxy, connections) = counting_proxy();
    let checkout = base.path().join("checkout");
    std::fs::create_dir(&checkout).unwrap();
    let init = Command::new("git")
        .args(["init", "-q"])
        .arg(&checkout)
        .status()
        .unwrap();
    assert!(init.success());
    let bin = base.path().join("bin");
    fake_language_server(&bin);
    let path = std::env::var_os("PATH").unwrap_or_default();
    let path =
        std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&path))).unwrap();

    let out = demo_command(base.path(), &proxy, &["--output", "checkout/run", "--keep"])
        .env("PATH", path)
        .output()
        .unwrap();

    assert_passed(&out);
    assert_eq!(connections.load(Ordering::SeqCst), 0, "the demo connected");
    let home = checkout.join("run").join("home");
    assert!(home.join("profiles").join("default").exists());
    // A started server records its status under the profile's LSP cache;
    // an installed one lands under runtimes/lsp.
    assert!(!home.join("profiles/default/cache/lsp").exists());
    assert!(!home.join("runtimes/lsp").exists());
}
