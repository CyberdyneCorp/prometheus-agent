//! The real pyright (spec: lsp-diagnostics, Diagnostics introduced by an
//! edit): installed with npm into the fixture's `runtimes/lsp/` on first
//! use, run sandboxed, it reports an undefined name a patch introduced.
//! CI sets `PMT_TEST_LSP=require`; elsewhere the test skips unless npm is
//! installed and `PMT_TEST_LSP=require` (it downloads pyright). The
//! download is the network's pace, not the product's: the test waits up
//! to [`INSTALL_BOUND`] for it, and nextest retries it (`.config/
//! nextest.toml`) when the registry fails.

use std::fs;
use std::time::{Duration, Instant};

use pmt_tools::lsp::{LspSetup, SessionDiagnostics, Toolchain};

use crate::sandbox::sandboxed;
use crate::support::{Fixture, findings, patch, read, write};

/// How long the first edit may take to install and warm pyright: one
/// `wait_timeout_ms` (60 s) was too short for an npm install on a slow
/// CI runner (Intel macOS).
const INSTALL_BOUND: Duration = Duration::from_mins(10);

#[tokio::test(flavor = "multi_thread")]
async fn pyright_reports_an_undefined_name_after_a_patch() {
    if std::env::var("PMT_TEST_LSP").as_deref() != Ok("require") {
        eprintln!("skipped: set PMT_TEST_LSP=require to install and run the real pyright");
        return;
    }
    // pyright's stderr and the session's routing are logged at DEBUG: a
    // failure shows why the server said nothing (nextest runs each test in
    // its own process, so the global subscriber is this test's alone).
    let _ = tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_test_writer()
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish(),
    );
    let fx = Fixture::new();
    let config: pmt_config::schema::LspConfig =
        toml::from_str("wait_timeout_ms = 60000\nwarmup_timeout_ms = 120000\n").unwrap();
    let workspaces =
        pmt_tools::lsp::workspace::Workspaces::new(&pmt_tools::lsp::workspace::TrustInputs {
            start_dir: &fx.app,
            automatic: true,
            user_home: Some(&fx.home),
            exclude_roots: &[],
            trusted_workspaces: &[],
        });
    let lsp = SessionDiagnostics::new(
        LspSetup {
            installs: pmt_tools::lsp::install::Installs::of_home(
                &fx.prometheus,
                std::env::var_os("PATH"),
            ),
            config,
            profile_dir: fx.profile.clone(),
            session_id: "ses_pyright".into(),
            workspaces,
            launcher: sandboxed(&fx),
            toolchain: Toolchain {
                allow_lazy_install: true,
                ..Toolchain::default()
            },
            handle: tokio::runtime::Handle::current(),
        },
        true,
    )
    .unwrap();
    let files = fx.files(&fx.app, Some(lsp.clone()));
    let app = fx.app.join("app.py");
    // The first edit installs pyright and warms it; each settle waits one
    // `wait_timeout_ms` for the check, so wait again while it still runs.
    write(&files, &app, "import os\n\nprint(os.name)\n").await;
    let deadline = Instant::now() + INSTALL_BOUND;
    loop {
        let _ = lsp.settle().await;
        if !lsp.has_pending() || Instant::now() > deadline {
            break;
        }
    }
    assert!(
        fx.prometheus
            .join("runtimes/lsp/pyright/node_modules/.bin/pyright-langserver")
            .exists(),
        "pyright was installed into runtimes/lsp"
    );
    read(&files, &app).await;
    let result = patch(&files, &app, "print(os.name)\n", "print(os.name)\nfoo()\n").await;
    let found = findings(&result, &lsp).await;
    assert!(
        found.contains("\"foo\" is not defined"),
        "{result}\n{found}"
    );
    assert!(found.contains("(pyright)"), "{found}");
    assert_eq!(found.matches("is not defined").count(), 1, "{found}");
    let _ = fs::remove_dir_all(fx.prometheus.join("runtimes"));
}
