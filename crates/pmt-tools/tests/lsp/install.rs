//! Server installation (spec: lsp-diagnostics, Server installation): the
//! configured package manager or none, into `runtimes/lsp/<id>/`.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use pmt_config::schema::{LspConfig, LspInstallStrategy, LspPackageManager};
use pmt_tools::lsp::install::{Installs, Source, Tools};
use pmt_tools::lsp::registry::Registry;

/// A stand-in `npm` that records its arguments and "installs" the
/// requested server's binary under `--prefix`.
fn fake_npm(bin: &Path, record: &Path) {
    fs::create_dir_all(bin).unwrap();
    let script = format!(
        "#!/bin/sh\necho \"$@\" >> '{}'\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = --prefix ]; then prefix=$2; fi\n  shift\ndone\n\
         /bin/mkdir -p \"$prefix/node_modules/.bin\"\nprintf '#!/bin/sh\\n' > \"$prefix/node_modules/.bin/pyright-langserver\"\n\
         /bin/chmod +x \"$prefix/node_modules/.bin/pyright-langserver\"\n",
        record.display()
    );
    let npm = bin.join("npm");
    fs::write(&npm, script).unwrap();
    fs::set_permissions(&npm, fs::Permissions::from_mode(0o755)).unwrap();
}

fn tools(manager: LspPackageManager) -> Tools<'static> {
    Tools {
        manager,
        node: None,
        python: None,
    }
}

#[tokio::test]
async fn a_missing_pnpm_skips_the_install_and_never_falls_back_to_npm() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let bin = dir.path().join("bin");
    let record = dir.path().join("npm-calls");
    fake_npm(&bin, &record);
    let installs = Installs::of_home(&dir.path().join("home"), Some(bin.into_os_string()));
    let registry = Registry::new(&LspConfig::default());
    let pyright = registry.get("pyright").unwrap();
    let err = installs
        .install(pyright, &tools(LspPackageManager::Pnpm))
        .await
        .unwrap_err();
    assert!(err.contains("pnpm is not installed"), "{err}");
    assert!(err.starts_with("skipped:"), "{err}");
    assert!(!record.exists(), "npm was used");
    assert_eq!(installs.resolve(pyright, LspInstallStrategy::Auto), None);
}

#[tokio::test]
async fn npm_installs_a_server_into_the_runtimes_lsp_directory() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let bin = dir.path().join("bin");
    let record = dir.path().join("npm-calls");
    fake_npm(&bin, &record);
    let home = dir.path().join("home");
    let installs = Installs::of_home(&home, Some(bin.into_os_string()));
    let registry = Registry::new(&LspConfig::default());
    let pyright = registry.get("pyright").unwrap();
    let exe = installs
        .install(pyright, &tools(LspPackageManager::Npm))
        .await
        .unwrap();
    assert!(
        exe.starts_with(home.join("runtimes/lsp/pyright")),
        "{}",
        exe.display()
    );
    let calls = fs::read_to_string(&record).unwrap();
    assert!(calls.contains("install --prefix"), "{calls}");
    assert!(calls.contains("--ignore-scripts pyright@"), "{calls}");
    let resolved = installs.resolve(pyright, LspInstallStrategy::Auto).unwrap();
    assert_eq!((resolved.program, resolved.source), (exe, Source::Managed));
    assert_eq!(resolved.args, ["--stdio"]);
    // `manual` uses only binaries on PATH.
    assert_eq!(installs.resolve(pyright, LspInstallStrategy::Manual), None);
}

#[tokio::test]
async fn manual_and_custom_servers_are_never_installed() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let installs = Installs::of_home(dir.path(), None);
    let config: LspConfig =
        toml::from_str("[servers.mylang]\ncommand = [\"mylang-ls\"]\nextensions = [\".ml2\"]\n")
            .unwrap();
    let registry = Registry::new(&config);
    let manual = installs
        .install(
            registry.get("rust-analyzer").unwrap(),
            &tools(LspPackageManager::Npm),
        )
        .await
        .unwrap_err();
    assert!(
        manual.contains("rustup component add rust-analyzer"),
        "{manual}"
    );
    let custom = installs
        .install(
            registry.get("mylang").unwrap(),
            &tools(LspPackageManager::Npm),
        )
        .await
        .unwrap_err();
    assert!(custom.contains("custom server"), "{custom}");
}
