//! Language servers run sandboxed (spec: lsp-diagnostics, Language
//! servers run sandboxed): a server reads its workspace but cannot write
//! it, and the edit's result does not depend on what it tried. The
//! stand-in probes its client's pid like vscode-languageserver servers do,
//! so a client that names one fails here under Seatbelt (macOS).

use std::fs;
use std::path::Path;
use std::sync::Arc;

use pmt_core::env::{EnvPolicy, LaunchEnv};
use pmt_tools::lsp::launch::{Launcher, SandboxLauncher};
use pmt_tools::terminal::env::{ChildEnv, EnvRequest};

use crate::support::{Fixture, findings, write};

pub(crate) fn sandboxed(fx: &Fixture) -> Arc<dyn Launcher> {
    let launch = LaunchEnv::from_vars(std::env::vars_os(), &EnvPolicy::default());
    Arc::new(SandboxLauncher {
        prometheus_home: fx.prometheus.clone(),
        user_home: fx.home.clone(),
        sandbox: pmt_config::schema::SandboxConfig::default(),
        env: ChildEnv::build(&launch, EnvRequest::default()),
        egress: None,
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_cannot_write_its_workspace_and_the_edit_is_unaffected() {
    let fx = Fixture::new();
    // The stand-in must be readable inside the sandbox: copy it into the
    // workspace it may read.
    let tools = fx.app.join("tools");
    fs::create_dir_all(&tools).unwrap();
    let script = tools.join("fake_lsp.py");
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/lsp/fake_lsp.py"),
        &script,
    )
    .unwrap();
    let escape = fx.app.join("target/escape");
    let config: pmt_config::schema::LspConfig = toml::from_str(&format!(
        "wait_timeout_ms = 10000\n[servers.pyright]\ncommand = [\"python3\", \"{}\"]\nenv = {{ FAKE_LSP_WRITE = \"{}\" }}\n",
        script.display(),
        escape.display()
    ))
    .unwrap();
    let lsp = fx.diagnostics_with(config, &fx.app, true, sandboxed(&fx));
    let files = fx.files(&fx.app, Some(lsp.clone()));
    let app = fx.app.join("app.py");
    let result = write(&files, &app, "BAD_x\n").await;
    assert_eq!(result["created"], true, "{result}");
    assert_eq!(fs::read_to_string(&app).unwrap(), "BAD_x\n");
    let found = findings(&result, &lsp).await;
    assert!(
        found.contains("\"x\" is not defined"),
        "the server ran: {found}"
    );
    assert!(found.contains("write denied"), "{found}");
    assert!(!escape.exists(), "the server wrote into the workspace");
}
