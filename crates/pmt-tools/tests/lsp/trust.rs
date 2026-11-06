//! Workspace gating and trust (spec: lsp-diagnostics, Workspace gating;
//! Workspace trust).

use std::fs;
use std::io::Write as _;
use std::sync::{Arc, Mutex};

use pmt_tools::lsp::Route;

use crate::support::{Fixture, findings, repo, write};

/// Log lines written while `f` runs on this thread.
fn logged(f: impl FnOnce()) -> String {
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let buffer = Buffer::default();
    let writer = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    let mut out = buffer.0.lock().unwrap().clone();
    let _ = out.flush();
    String::from_utf8(out).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gateway_session_in_the_home_directory_starts_no_server() {
    let fx = Fixture::new();
    let lsp = fx.diagnostics(fx.config(""), &fx.home);
    let files = fx.files(&fx.home, Some(lsp.clone()));
    let result = write(&files, &fx.home.join("notes.py"), "print(BAD_x)\n").await;
    assert_eq!(result["lint"]["status"], "ok", "{result}");
    assert!(result.get("lsp_diagnostics").is_none(), "{result}");
    assert_eq!(lsp.route(&fx.home.join("notes.py")), Route::None);
    assert_eq!(lsp.settle().await, None);
    assert_eq!(fx.starts(), 0, "{}", fx.log());
}

#[test]
fn a_fresh_clone_does_not_start_a_server_that_runs_project_code_and_says_why() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _guard = runtime.enter();
    let fx = Fixture::new();
    let clone = repo(&fx.base.join("tmp/x"));
    let lsp = fx.diagnostics(fx.config_for("rust-analyzer", "", "", ""), &fx.app);
    let files = fx.files(&fx.app, Some(lsp.clone()));
    let build = clone.join("build.rs");
    let log = logged(|| {
        let result = files.write_file(&build.display().to_string(), "fn main() { BAD_x(); }\n");
        assert!(
            !result.text_content().contains("lsp_diagnostics"),
            "{}",
            result.text_content()
        );
    });
    let line = format!(
        "skipped: untrusted workspace {}; add it to lsp.trusted_workspaces",
        clone.display()
    );
    assert!(log.contains(&line), "{log}");
    assert_eq!(lsp.route(&build), Route::Skipped(line));
    assert!(!lsp.has_pending());
    assert_eq!(fx.starts(), 0, "{}", fx.log());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_explicitly_trusted_workspace_may_start_rust_analyzer() {
    let fx = Fixture::new();
    let lib = repo(&fx.home.join("code/lib"));
    fs::create_dir_all(lib.join("src")).unwrap();
    let config = fx.config_for("rust-analyzer", "", "", "trusted_workspaces = [\"~/code\"]");
    let lsp = fx.diagnostics(config, &fx.app);
    let file = lib.join("src/lib.rs");
    assert!(matches!(
        lsp.route(&file),
        Route::Check { trusted: true, .. }
    ));
    let files = fx.files(&fx.app, Some(lsp.clone()));
    let result = write(&files, &file, "pub fn a() { BAD_b(); }\n").await;
    let found = findings(&result, &lsp).await;
    assert!(found.contains("(rust-analyzer)"), "{found}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_untrusted_workspace_starts_only_servers_that_run_no_project_code() {
    let fx = Fixture::new();
    // A cron job gets no automatic trust, even in its own directory.
    let config = fx.config_for(
        "bash-language-server",
        "",
        "",
        "[servers.rust-analyzer]\ncommand = [\"rust-analyzer\"]",
    );
    let lsp = fx.diagnostics_with(
        config,
        &fx.app,
        false,
        Arc::new(pmt_tools::lsp::launch::Unconfined),
    );
    assert!(matches!(lsp.route(&fx.app.join("a.rs")), Route::Skipped(_)));
    let files = fx.files(&fx.app, Some(lsp.clone()));
    let result = write(&files, &fx.app.join("run.sh"), "echo BAD_x\n").await;
    let found = findings(&result, &lsp).await;
    assert!(found.contains("(bash-language-server)"), "{found}");
}

#[tokio::test(flavor = "multi_thread")]
async fn svelte_starts_untrusted_with_project_code_off() {
    let fx = Fixture::new();
    let config = fx.config_for("svelte-language-server", "", "", "");
    let lsp = fx.diagnostics_with(
        config,
        &fx.app,
        false,
        Arc::new(pmt_tools::lsp::launch::Unconfined),
    );
    let files = fx.files(&fx.app, Some(lsp.clone()));
    write(&files, &fx.app.join("App.svelte"), "<p>hi</p>\n").await;
    let _ = lsp.settle().await;
    assert!(fx.log().contains("\"isTrusted\": false"), "{}", fx.log());
}

#[tokio::test(flavor = "multi_thread")]
async fn excluded_roots_start_no_server() {
    let fx = Fixture::new();
    let lsp = fx.diagnostics(fx.config("exclude_roots = [\"~/work\"]"), &fx.app);
    assert_eq!(lsp.route(&fx.app.join("a.py")), Route::None);
    let files = fx.files(&fx.app, Some(lsp.clone()));
    write(&files, &fx.app.join("a.py"), "BAD_x\n").await;
    assert_eq!(lsp.settle().await, None);
    assert_eq!(fx.starts(), 0);
}
