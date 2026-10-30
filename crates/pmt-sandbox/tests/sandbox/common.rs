//! A fake user home with credential stores, a workspace inside it and a
//! directory outside it, all under the target directory.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pmt_config::schema::SandboxConfig;
use pmt_sandbox::{Options, Paths, Policy, Sandbox};

pub(crate) const SECRET: &str = "PRIVATE-KEY-MATERIAL";

pub(crate) struct Fixture {
    _root: tempfile::TempDir,
    pub(crate) home: PathBuf,
    pub(crate) work: PathBuf,
    pub(crate) outside: PathBuf,
    pub(crate) paths: Paths,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let base = fs::canonicalize(root.path()).unwrap();
        let home = base.join("home");
        let work = home.join("work");
        let outside = base.join("outside");
        let scratch = home.join(".prometheus/scratch/s1");
        for dir in [
            &work,
            &outside,
            &scratch,
            &home.join(".ssh"),
            &home.join(".aws"),
        ] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(home.join(".ssh/id_rsa"), SECRET).unwrap();
        fs::write(home.join(".ssh/id_ed25519"), SECRET).unwrap();
        fs::write(home.join(".aws/credentials"), SECRET).unwrap();
        fs::write(home.join("notes.txt"), "public notes").unwrap();
        fs::write(home.join(".prometheus/config.toml"), SECRET).unwrap();
        let paths = Paths {
            user_home: home.clone(),
            prometheus_home: home.join(".prometheus"),
            workspace: work.clone(),
            scratch: vec![scratch],
        };
        Self {
            _root: root,
            home,
            work,
            outside,
            paths,
        }
    }

    /// The baseline plus `extra` capabilities.
    pub(crate) fn policy(&self, extra: &[&str]) -> Policy {
        let caps: Vec<&str> = pmt_sandbox::BASELINE
            .iter()
            .copied()
            .chain(extra.iter().copied())
            .collect();
        Policy::from_capabilities(&caps, &self.paths).unwrap()
    }
}

/// A sandbox for `policy` with the default `[security.sandbox]` settings.
pub(crate) fn confine(policy: Policy) -> Sandbox {
    let options = Options {
        launcher: Some(launcher()),
        report: None,
    };
    Sandbox::with_options(policy, &SandboxConfig::default(), options).unwrap()
}

/// The launcher executable built with this crate (used on Windows).
pub(crate) fn launcher() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_pmt-sandbox-launch"))
}

/// Collects formatted tracing output.
#[derive(Clone, Default)]
pub(crate) struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Runs `f` with WARN+ events captured.
pub(crate) fn capture_warnings<T>(f: impl FnOnce() -> T) -> (T, String) {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let value = tracing::subscriber::with_default(subscriber, f);
    (value, captured.text())
}
