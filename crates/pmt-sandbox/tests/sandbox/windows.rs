//! The AppContainer + Job Object backend on a real Windows host
//! (spec: security-boundaries, OS sandbox per platform).

use std::path::Path;
use std::process::Output;

use pmt_sandbox::{Feature, Level, Sandbox};

use crate::common::{Fixture, SECRET, confine};

/// Runs `cmd /C <line>` confined, from `cwd` (a directory the container may
/// use, as the terminal backend always sets).
fn cmd(sandbox: &Sandbox, cwd: &Path, line: &[&str]) -> Output {
    let mut command = sandbox.command("cmd.exe").unwrap();
    command.current_dir(cwd).arg("/C").args(line);
    command.output().unwrap()
}

fn write(sandbox: &Sandbox, cwd: &Path, path: &Path) -> Output {
    cmd(sandbox, cwd, &["echo", "x>", path.to_str().unwrap()])
}

#[test]
fn writes_outside_the_workspace_fail() {
    let fixture = Fixture::new();
    let sandbox = confine(fixture.policy(&[]));
    let inside = fixture.work.join("made-here.txt");
    let output = write(&sandbox, &fixture.work, &inside);
    assert!(output.status.success(), "{output:?}");
    assert!(inside.exists());

    let outside = fixture.outside.join("escaped.txt");
    let output = write(&sandbox, &fixture.work, &outside);
    assert!(!output.status.success(), "{output:?}");
    assert!(!outside.exists());
}

#[test]
fn credential_stores_stay_denied_under_a_broad_read_grant() {
    let fixture = Fixture::new();
    let sandbox = confine(fixture.policy(&["fs.read:~/**"]));
    let notes = fixture.home.join("notes.txt");
    let output = cmd(&sandbox, &fixture.work, &["type", notes.to_str().unwrap()]);
    assert!(output.status.success(), "{output:?}");
    for store in [".ssh\\id_rsa", ".aws\\credentials"] {
        let path = fixture.home.join(store);
        let output = cmd(&sandbox, &fixture.work, &["type", path.to_str().unwrap()]);
        assert!(!output.status.success(), "{output:?}");
        assert!(!String::from_utf8_lossy(&output.stdout).contains(SECRET));
    }
}

#[test]
fn report_strength_per_feature() {
    let fixture = Fixture::new();
    let sandbox = confine(fixture.policy(&[]));
    let report = sandbox.report();
    assert_eq!(report.platform, "appcontainer");
    let levels: Vec<(Feature, Level)> = report
        .features
        .iter()
        .map(|f| (f.feature, f.level))
        .collect();
    assert_eq!(
        levels,
        [
            (Feature::Fs, Level::Enforced),
            (Feature::Net, Level::Enforced),
            (Feature::Process, Level::Enforced),
            (Feature::Syscalls, Level::Partial),
        ]
    );
    assert_eq!(report.enforcement().to_string(), "full");
}

fn acl(path: &Path) -> String {
    let output = std::process::Command::new("icacls")
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn acl_entries_are_revoked_when_the_sandbox_ends() {
    let fixture = Fixture::new();
    let sub = fixture.work.join("sub");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join("file.txt"), "x").unwrap();
    let objects = [fixture.work.clone(), sub.clone(), sub.join("file.txt")];
    let before: Vec<String> = objects.iter().map(|p| entries(&acl(p))).collect();
    let sandbox = confine(fixture.policy(&[]));
    let during: Vec<String> = objects.iter().map(|p| acl(p)).collect();
    // AppContainer SIDs start with S-1-15-2-; the grant reaches children.
    assert!(
        during.iter().all(|a| a.contains("S-1-15-2-")),
        "{during:#?}"
    );
    drop(sandbox);
    let after: Vec<String> = objects.iter().map(|p| entries(&acl(p))).collect();
    assert!(after.iter().all(|a| !a.contains("S-1-15-2-")), "{after:#?}");
    assert_eq!(after, before, "the same principals keep the same rights");
}

/// The (principal, rights) entries of an `icacls` listing, without the
/// `(I)` inherited marker: when the parent directory's DACL is not marked
/// auto-inherited (seen on the Windows ARM runners), Windows rewrites
/// inherited entries as explicit copies whenever the DACL is set.
fn entries(listing: &str) -> String {
    listing.replace("(I)", "")
}

/// Egress for a proxied child (spec: security-boundaries, Egress
/// enforcement for child processes): an `AppContainer` cannot reach the
/// loopback proxy, so the child runs (net `partial`, the platform's
/// maximum, not a refused downgrade), reaches no loopback address, and
/// reaches public addresses (`internetClient`) only while its relay admits
/// every public host, asked again at every start.
#[test]
fn a_proxied_child_gets_public_internet_only_while_the_policy_admits_it() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let fixture = Fixture::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let public = Arc::new(AtomicBool::new(false));
    let gate = Arc::clone(&public);
    let relay = pmt_sandbox::Relay::new(|_| {}).with_direct(move || gate.load(Ordering::SeqCst));
    let sandbox = confine(fixture.policy(&[]).through_proxy(port, relay));
    let net = &sandbox.report().features[1];
    assert_eq!((net.feature, net.level), (Feature::Net, Level::Partial));
    assert!(net.gap.is_none(), "{net:?}");

    let connect = |host: &str, port: u16| {
        let line = format!(
            "try {{ (New-Object Net.Sockets.TcpClient).Connect('{host}', {port}); 'connected' }} catch {{ 'refused: ' + $_.Exception.GetBaseException().Message }}"
        );
        let output = sandbox
            .command("powershell.exe")
            .unwrap()
            .current_dir(&fixture.work)
            .args(["-NoProfile", "-NonInteractive", "-Command", &line])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        (text.starts_with("connected"), format!("{text} {output:?}"))
    };
    // github.com: a public host every CI runner reaches. The gate is asked
    // at every start, both ways.
    let mut seen = Vec::new();
    for admitted in [true, false, true] {
        public.store(admitted, Ordering::SeqCst);
        let (reached, why) = connect("github.com", 443);
        seen.push((admitted, reached, why));
    }
    assert!(seen.iter().all(|(a, r, _)| a == r), "{seen:#?}");
    let (reached, why) = connect("127.0.0.1", port);
    assert!(!reached, "{why}");
    assert!(listener.accept().is_err(), "the child reached loopback");
}
