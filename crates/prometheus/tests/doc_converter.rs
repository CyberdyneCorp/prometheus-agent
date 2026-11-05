//! The `doc-converter` runtime protocol (spec: execution-environments,
//! `read_file` document extraction): a scanned PDF read through a
//! converter executable on `PATH`.
#![cfg(unix)] // the fake converter is a shell script

use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;

use pmt_tools::browser::RuntimePolicy;
use pmt_tools::browser::launch::NoRuntimes;
use pmt_tools::files::documents::{Converted, DocConverter};
use prometheus::chat::session::documents::RuntimeConverter;

#[tokio::test(flavor = "multi_thread")]
async fn a_converter_on_path_answers_with_pages() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let script = dir.path().join("doc-converter");
    std::fs::write(
        &script,
        "#!/bin/sh\n[ \"$1\" = --json ] || exit 2\n[ -f \"$2\" ] || exit 3\nprintf '{\"pages\": [\"Intro\", \"\", \"End\"]}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let pdf = dir.path().join("scan.pdf");
    std::fs::write(&pdf, b"%PDF-1.7").unwrap();
    let converter = RuntimeConverter::new(
        RuntimePolicy {
            runtimes: Arc::new(NoRuntimes),
            allow_lazy_install: false,
            host_candidates: vec![script],
        },
        tokio::runtime::Handle::current(),
    );
    let pages = tokio::task::spawn_blocking(move || converter.convert(&pdf, "pdf"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        pages,
        Converted::Pages(vec!["Intro".into(), String::new(), "End".into()])
    );
}
