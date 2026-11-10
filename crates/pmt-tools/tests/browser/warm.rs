//! One warm start of the host browser per test run, before any test's own.
//!
//! The first start of the browser that ships with a CI runner image is
//! slow: its files come in from a cold disk. On the `x86_64` Linux and macOS
//! runners the first two browser tests of a job took 20 to 23 s and every
//! later start under 5 s, while the arm64 Linux runners (whose Chromium was
//! downloaded moments before, so its files are in memory) start in under
//! 1 s. A test's start is bounded by the product's 30 s launch timeout, so
//! a cold start on a busy runner failed with "the browser did not start
//! within 30 s". The first test process of a run starts the browser once,
//! with a generous bound, while the others wait on a lock; every test then
//! starts a warm browser, and the product timeout stays what it is.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pmt_config::schema::{DegradedFeature, SandboxConfig, SandboxMode};

use crate::support::Fixture;

/// Bound on the warm-up start: a cold start, with room to spare.
const WARM_TIMEOUT: Duration = Duration::from_mins(3);

/// Warms `executable` once per test run: per `NEXTEST_RUN_ID` (nextest
/// runs each test in its own process), or once per process under
/// `cargo test`.
pub(crate) fn once(executable: &Path) {
    let run = std::env::var("NEXTEST_RUN_ID")
        .unwrap_or_else(|_| format!("process {}", std::process::id()));
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("browser-warm-up");
    warm(executable, &dir, &run, WARM_TIMEOUT);
}

/// Starts and stops `executable` unless a process sharing `dir` already
/// did for `run`; answers whether this call started it. Callers for the
/// same `dir` wait for each other.
pub(crate) fn warm(executable: &Path, dir: &Path, run: &str, timeout: Duration) -> bool {
    std::fs::create_dir_all(dir).unwrap();
    let lock = File::create(dir.join("lock")).unwrap();
    // Held until `lock` drops at the end of this call.
    pmt_core::file_lock::lock(&lock).unwrap();
    let marker = dir.join("warmed");
    let stamp = format!("{run}\n{}", executable.display());
    if std::fs::read_to_string(&marker).is_ok_and(|text| text == stamp) {
        return false;
    }
    start(executable.to_path_buf(), timeout);
    std::fs::write(&marker, stamp).unwrap();
    true
}

/// One start of `executable` on a throwaway profile. A failure is only
/// printed: the tests that use the browser report it themselves.
fn start(executable: PathBuf, timeout: Duration) {
    // The caller may be inside a test's runtime: the start gets its own.
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let fixture = Fixture::new();
            let profiles = fixture.browser_cache().join("profiles");
            // The warm-up is about a cold disk, not isolation; the fake
            // browsers of these tests log outside any sandbox.
            let unconfined = SandboxConfig {
                mode: SandboxMode::Off,
                allow_degraded: vec![DegradedFeature::All],
            };
            match fixture
                .launch(&executable, &profiles, timeout, unconfined)
                .await
            {
                Ok((launched, _proxy)) => launched.terminate().await,
                Err(why) => eprintln!("browser warm-up: {why}"),
            }
        });
    })
    .join()
    .unwrap();
}

/// A fake browser that logs each start to `log`, then publishes its
/// DevTools endpoint and waits to be killed.
fn fake_browser(dir: &Path, log: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let fake = dir.join("fake-browser");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nfor arg in \"$@\"; do\n  case \"$arg\" in --user-data-dir=*) dir=\"${{arg#--user-data-dir=}}\" ;; esac\ndone\n\
             echo started >> '{}'\ndd bs=1 count=1 <&3 >/dev/null 2>&1\nprintf '{{\"id\":1,\"result\":{{}}}}\\000' >&4\nexec sleep 600\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    fake
}

#[test]
fn the_browser_is_warmed_once_per_test_run() {
    // The deflake of the cold first start: however many test processes of
    // one run ask at once, the browser starts once; a new run warms again.
    let fixture = Fixture::new();
    let log = fixture.home.join("starts");
    let fake = fake_browser(&fixture.home, &log);
    let dir = fixture.home.join("warm-up");
    let timeout = Duration::from_secs(10);
    let callers: Vec<_> = (0..4)
        .map(|_| {
            let (fake, dir) = (fake.clone(), dir.clone());
            std::thread::spawn(move || warm(&fake, &dir, "run 1", timeout))
        })
        .collect();
    let started = callers
        .into_iter()
        .map(|caller| caller.join().unwrap())
        .filter(|started| *started)
        .count();
    assert_eq!(started, 1);
    assert!(!warm(&fake, &dir, "run 1", timeout));
    assert!(warm(&fake, &dir, "run 2", timeout));
    let starts = std::fs::read_to_string(&log).unwrap();
    assert_eq!(starts.lines().count(), 2, "{starts}");
}
