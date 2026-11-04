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
/// enforcement for child processes): the child holds no network
/// capability and reaches the loopback proxy only through the relay in its
/// own container, at the address its proxy variables are rewritten to;
/// connecting to the proxy directly or to a public host is refused, and
/// `net` is reported `enforced`.
#[test]
fn a_proxied_child_reaches_only_the_proxy() {
    use std::io::Write as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let fixture = Fixture::new();
    let proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = proxy.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&accepted);
    std::thread::spawn(move || {
        for stream in proxy.incoming() {
            let Ok(mut stream) = stream else { continue };
            count.fetch_add(1, Ordering::SeqCst);
            let _ = stream.write_all(b"proxy\n");
        }
    });
    let sandbox = confine(
        fixture
            .policy(&[])
            .through_proxy(port, pmt_sandbox::Relay::new(|_| {})),
    );
    let net = &sandbox.report().features[1];
    assert_eq!((net.feature, net.level), (Feature::Net, Level::Enforced));
    assert!(net.gap.is_none(), "{net:?}");

    let run = |script: &str| {
        let output = sandbox
            .command("powershell.exe")
            .unwrap()
            .current_dir(&fixture.work)
            .env("HTTPS_PROXY", format!("http://127.0.0.1:{port}"))
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .output()
            .unwrap();
        (
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            format!("{output:?}"),
        )
    };
    let connect = |target: &str| {
        let line = format!(
            "try {{ $c = [Net.Sockets.TcpClient]::new(); $c.Connect({target}); 'connected' }} catch {{ 'refused: ' + $_.Exception.GetBaseException().GetType().Name }}"
        );
        run(&line)
    };
    // Through the proxy variable: the relay, then the proxy.
    let (through, why) = run(
        "$u = [Uri]$env:HTTPS_PROXY; $c = [Net.Sockets.TcpClient]::new(); $c.Connect($u.Host, $u.Port); $b = [byte[]]::new(6); $n = $c.GetStream().Read($b, 0, 6); \"$($u.Port) $([Text.Encoding]::ASCII.GetString($b, 0, $n).Trim())\"",
    );
    let (relay_port, answer) = through.split_once(' ').unwrap_or_default();
    assert_eq!(answer, "proxy", "{why}");
    assert_ne!(relay_port, port.to_string(), "{why}");
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
    // Around it: the proxy's own address, and a public host.
    for target in [
        format!("'127.0.0.1', {port}"),
        "'github.com', 443".to_owned(),
    ] {
        let (text, why) = connect(&target);
        assert_eq!(text, "refused: SocketException", "{target}: {why}");
    }
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        1,
        "the child bypassed the relay"
    );
}
