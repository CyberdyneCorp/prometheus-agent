//! The file tools refuse the credential stores the OS sandbox denies, even
//! under a grant covering the whole home, unless a grant names a path in
//! the store (spec: security-boundaries, Default sandbox denies credential
//! stores).

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use pmt_core::capability::{CapabilitySet, Places};
use pmt_sandbox::{Paths, Policy};
use pmt_tools::files::search::SearchArgs;
use pmt_tools::files::{ControlPaths, FileSession, FileSetup, Linter, LocalBackend, ReadArgs};
use pmt_tools::result::Status;

struct Home {
    _root: tempfile::TempDir,
    home: PathBuf,
}

impl Home {
    /// A user home holding AWS credentials, an SSH directory and a
    /// Prometheus profile with a secret.
    fn new() -> Self {
        let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let home = dunce::canonicalize(root.path()).unwrap().join("home");
        let profile = home.join(".prometheus/profiles/default");
        for dir in [home.join(".aws"), home.join(".ssh"), profile.clone()] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(home.join(".aws/credentials"), "aws_secret = sk-aws-1\n").unwrap();
        fs::write(profile.join("secrets.toml"), "KEY = \"sk-prom-2\"\n").unwrap();
        fs::write(home.join("notes.txt"), "sk-note-3\n").unwrap();
        Self { _root: root, home }
    }

    /// A session working in the home itself, granted the whole home
    /// (`fs.read`/`fs.write` on `~/**`) plus `extra`.
    fn session(&self, extra: &[String]) -> FileSession {
        let root = self.home.display().to_string().replace('\\', "/");
        let capabilities = CapabilitySet::parse(
            [format!("fs.read:{root}/**"), format!("fs.write:{root}/**")]
                .into_iter()
                .chain(extra.iter().cloned()),
        )
        .unwrap();
        let paths = Paths {
            user_home: self.home.clone(),
            prometheus_home: self.home.join(".prometheus"),
            workspace: self.home.clone(),
            scratch: Vec::new(),
        };
        let policy = Arc::new(Policy::from_set(&capabilities, &paths));
        let places = Places {
            user_home: self.home.clone(),
            workspace: self.home.clone(),
            scratch: Vec::new(),
            toolchain: Vec::new(),
        };
        let cwd = self.home.clone();
        FileSession::new(FileSetup {
            backend: Arc::new(LocalBackend::new(capabilities, places).denying_stores(policy)),
            cwd: Arc::new(move || cwd.clone()),
            user_home: Some(self.home.clone()),
            control: ControlPaths {
                prometheus_home: self.home.join(".prometheus"),
            },
            read_max_chars: pmt_tools::setting::fixed(100_000),
            linter: Linter::builtin(),
        })
    }
}

#[test]
fn stores_under_a_granted_home_are_refused() {
    let fx = Home::new();
    let session = fx.session(&[]);
    for path in [
        ".aws/credentials",
        ".prometheus/profiles/default/secrets.toml",
    ] {
        let result = session.read_file(&ReadArgs::new(path));
        assert_eq!(
            result.status,
            Status::Denied,
            "{path}: {}",
            result.text_content()
        );
        assert!(result.text_content().contains("credential store"), "{path}");
    }
    let found = session.search_files(&SearchArgs::new("sk-")).text_content();
    assert!(found.contains("notes.txt"), "{found}");
    assert!(
        !found.contains("sk-aws") && !found.contains("sk-prom"),
        "{found}"
    );
    assert!(
        !found.contains("credentials") && !found.contains("secrets.toml"),
        "{found}"
    );
    let written = session.write_file(".ssh/authorized_keys", "ssh-ed25519 AAAA x\n");
    assert_eq!(written.status, Status::Denied, "{}", written.text_content());
    assert!(!fx.home.join(".ssh/authorized_keys").exists());
    assert_eq!(
        session.read_file(&ReadArgs::new("notes.txt")).status,
        Status::Ok
    );
}

#[test]
fn a_grant_naming_the_store_path_allows_it() {
    let fx = Home::new();
    let file = fx.home.join(".aws/credentials");
    let grant = format!("fs.read:{}", file.display().to_string().replace('\\', "/"));
    let session = fx.session(&[grant]);
    let result = session.read_file(&ReadArgs::new(".aws/credentials"));
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    // Only that path: the rest of the store and other stores stay closed.
    let written = session.write_file(".aws/config", "[default]\n");
    assert_eq!(written.status, Status::Denied, "{}", written.text_content());
}
