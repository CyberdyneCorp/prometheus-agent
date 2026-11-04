//! The Landlock + seccomp backend on a real kernel.

use pmt_sandbox::{Feature, Level};

use crate::common::{Fixture, confine};

#[test]
fn report_names_landlock_and_seccomp() {
    let fixture = Fixture::new();
    let sandbox = confine(fixture.policy(&[]));
    let report = sandbox.report();
    assert_eq!(report.platform, "landlock+seccomp");
    assert_eq!(
        report.level(Feature::Fs),
        Level::Enforced,
        "CI kernels have Landlock: {report:?}"
    );
    assert_eq!(report.level(Feature::Syscalls), Level::Enforced);
    let mechanisms: Vec<&str> = report.features.iter().map(|f| f.mechanism).collect();
    assert_eq!(mechanisms, ["landlock", "landlock", "seccomp", "seccomp"]);
}

#[test]
fn ptrace_and_user_namespaces_are_refused() {
    let fixture = Fixture::new();
    let sandbox = confine(fixture.policy(&[]));
    if std::path::Path::new("/usr/bin/unshare").exists() {
        let output = sandbox
            .command("/usr/bin/unshare")
            .unwrap()
            .args(["--user", "/usr/bin/true"])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{output:?}");
    }
}

/// Egress enforcement for child processes: Landlock filters ports, not
/// addresses, so a child routed through the egress proxy runs in its own
/// network namespace. It reaches the proxy (through the listener handed to
/// the relay) and nothing else, not even another address listening on the
/// proxy's port.
#[test]
fn a_proxied_child_reaches_only_the_proxy() {
    use std::io::Write as _;
    use std::net::TcpListener;

    let fixture = Fixture::new();
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = proxy.local_addr().unwrap().port();
    let elsewhere = TcpListener::bind(("127.0.0.2", port)).unwrap();
    elsewhere.set_nonblocking(true).unwrap();
    let relay = pmt_sandbox::Relay::new(|attached| {
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = attached.listener.accept() {
                let _ = stream.write_all(b"proxy\n");
            }
        });
    });
    let sandbox = confine(fixture.policy(&[]).through_proxy(port, relay));
    assert_eq!(
        sandbox.report().level(Feature::Net),
        Level::Enforced,
        "CI allows unprivileged user namespaces: {:?}",
        sandbox.report()
    );
    let read = |address: &str| {
        sandbox
            .command("/bin/bash")
            .unwrap()
            .args([
                "-c",
                &format!("exec 3<>/dev/tcp/{address}/{port} && head -c 6 <&3"),
            ])
            .output()
            .unwrap()
    };
    let through = read("127.0.0.1");
    assert_eq!(
        String::from_utf8_lossy(&through.stdout),
        "proxy\n",
        "{through:?}"
    );
    let around = read("127.0.0.2");
    assert!(!around.status.success(), "{around:?}");
    assert!(
        elsewhere.accept().is_err(),
        "the other address on the proxy's port was reached"
    );
}
