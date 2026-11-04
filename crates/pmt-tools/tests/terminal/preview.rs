//! Approval explain previews run for real under a read-only, offline
//! sandbox (spec: tool-system, Approval explain and dry-run).

use std::sync::Arc;

use pmt_config::schema::SandboxConfig;
use pmt_sandbox::{Options, Policy, Sandbox};
use pmt_tools::approval::Finding;
use pmt_tools::approval::explain::{Previewer, explain};
use pmt_tools::terminal::env::{ChildEnv, EnvRequest};

use crate::common::Fixture;

fn previewer(fx: &Fixture) -> Previewer {
    let policy = Policy::from_capabilities(
        &[
            "fs.read:<workspace>/**",
            "fs.read:<toolchain>",
            "process.spawn:*",
        ],
        &fx.paths,
    )
    .unwrap();
    let sandbox =
        Sandbox::with_options(policy, &SandboxConfig::default(), Options::default()).unwrap();
    let env = ChildEnv::build(&fx.launch(&[]), EnvRequest::default());
    Previewer::new(Arc::new(sandbox), env, "/bin/sh".into())
}

/// Scenario: Preview cannot write.
#[tokio::test]
async fn a_preview_that_writes_is_denied_and_the_denial_is_shown() {
    let fx = Fixture::new();
    let output = previewer(&fx).run("echo hi > made.txt", &fx.work).await;
    assert!(!fx.work.join("made.txt").exists(), "{output}");
    let lower = output.to_lowercase();
    assert!(
        lower.contains("not permitted") || lower.contains("permission denied"),
        "{output}"
    );
    assert!(!output.contains("(exit status 0)"), "{output}");
}

/// Scenario: Explaining a recursive delete.
#[tokio::test]
async fn explaining_a_recursive_delete_lists_the_matched_paths_and_deletes_nothing() {
    let fx = Fixture::new();
    let build = fx.work.join("build");
    std::fs::create_dir_all(&build).unwrap();
    for name in ["a.o", "b.o"] {
        std::fs::write(build.join(name), "x").unwrap();
    }
    let finding = Finding {
        detector: "dangerous-pattern".into(),
        reason: "recursive delete".into(),
        fragment: "rm -rf".into(),
    };
    let text = explain(
        "rm -rf build/*",
        &[finding],
        &fx.work,
        Some(&previewer(&fx)),
    )
    .await;
    for name in ["a.o", "b.o"] {
        assert!(build.join(name).exists());
        assert!(
            text.contains(&build.join(name).display().to_string()),
            "{text}"
        );
    }
    // The preview ran in the sandbox and listed the same paths.
    assert!(text.contains("preview `ls -1d -- 'build/'*`"), "{text}");
    assert!(text.contains("  build/a.o\n  build/b.o\n"), "{text}");
    assert!(text.contains("(exit status 0)"), "{text}");
}
