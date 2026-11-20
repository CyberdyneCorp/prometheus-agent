//! External secret sources (spec: security-boundaries, External secret
//! sources are opt-in) against stub `bws` and `op` CLIs that log each run.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_core::secret::SecretString;

use super::external::ExternalSources;
use super::{SecretStore, SecretsHandle};
use crate::schema::{
    BitwardenSourceConfig, Config, OnePasswordSourceConfig, SecretName, SecretsBackend,
};
use crate::{Home, ProfileName};

struct Fixture {
    dir: tempfile::TempDir,
    store: SecretStore,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();
        let profile = home.profile(&ProfileName::default_profile()).unwrap();
        let store = SecretStore::open(&profile, SecretsBackend::File);
        Self { dir, store }
    }

    fn log(&self) -> PathBuf {
        self.dir.path().join("runs.log")
    }

    fn runs(&self) -> usize {
        std::fs::read_to_string(self.log()).map_or(0, |t| t.lines().count())
    }

    /// A stub CLI that logs its run and answers only token `good`.
    fn stub(&self, name: &str, variable: &str, answer: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        let script = format!(
            "#!/bin/sh\necho \"$0 $*\" >> '{log}'\n[ \"${variable}\" = good ] || exit 1\nprintf '%s' '{answer}'\n",
            log = self.log().display()
        );
        std::fs::write(&path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn handle(&self, sources: Option<ExternalSources>, launch: &[(&str, &str)]) -> SecretsHandle {
        let launch: BTreeMap<String, SecretString> = launch
            .iter()
            .map(|(k, v)| ((*k).to_owned(), SecretString::new(*v)))
            .collect();
        SecretsHandle::new("default", self.store.clone(), Some(launch))
            .with_external(sources.map(Arc::new))
    }
}

const LISTING: &str = r#"[{"key":"SHARED","value":"from-bitwarden"},{"key":"BULK_ONLY","value":"b1"},{"key":"BULK_ONLY","value":"b2"}]"#;

fn bitwarden(ttl: u64) -> BitwardenSourceConfig {
    BitwardenSourceConfig {
        enabled: true,
        project_id: Some("proj-1".to_owned()),
        cache_ttl_secs: ttl,
        ..BitwardenSourceConfig::default()
    }
}

fn onepassword() -> OnePasswordSourceConfig {
    OnePasswordSourceConfig {
        enabled: true,
        map: BTreeMap::from([("SHARED".to_owned(), "op://vault/item/field".to_owned())]),
        ..OnePasswordSourceConfig::default()
    }
}

/// Scenario: Default install. Both sources are off: nothing is built, so
/// neither CLI can run.
#[test]
fn a_default_config_builds_no_source() {
    let config = Config::default();
    assert!(!config.secrets.bitwarden.enabled && !config.secrets.onepassword.enabled);
    assert!(ExternalSources::for_profile(&config, Path::new("/nonexistent")).is_none());
}

#[cfg(unix)]
#[test]
fn mapped_entries_beat_bulk_ones_and_the_first_bulk_claim_wins() {
    let fx = Fixture::new();
    fx.store
        .set("BWS_ACCESS_TOKEN", &SecretString::new("good"))
        .unwrap();
    fx.store
        .set("OP_SERVICE_ACCOUNT_TOKEN", &SecretString::new("good"))
        .unwrap();
    let bws = fx.stub("bws", "BWS_ACCESS_TOKEN", LISTING);
    let op = fx.stub("op", "OP_SERVICE_ACCOUNT_TOKEN", "from-1password\n");
    let sources =
        ExternalSources::with_programs(Some((onepassword(), op)), Some((bitwarden(300), bws)));
    let handle = fx.handle(sources, &[]);
    assert_eq!(handle.resolve("SHARED").unwrap().expose(), "from-1password");
    assert_eq!(handle.resolve("BULK_ONLY").unwrap().expose(), "b1");
    // Cached: no further runs within the TTL.
    let runs = fx.runs();
    assert_eq!(handle.resolve("BULK_ONLY").unwrap().expose(), "b1");
    assert_eq!(handle.resolve("SHARED").unwrap().expose(), "from-1password");
    assert_eq!(fx.runs(), runs);
    let log = std::fs::read_to_string(fx.log()).unwrap();
    assert!(
        log.contains("bws secret list proj-1 --output json"),
        "{log}"
    );
    assert!(
        !log.contains("good"),
        "the token is never an argument: {log}"
    );
    // The store still wins over every external source.
    fx.store
        .set("SHARED", &SecretString::new("from-store"))
        .unwrap();
    assert_eq!(handle.resolve("SHARED").unwrap().expose(), "from-store");
    let redactor = handle.redactor().unwrap();
    assert_eq!(redactor.redact("x b1 y"), "x [REDACTED:BULK_ONLY] y");
}

#[cfg(unix)]
#[test]
fn the_token_comes_only_from_the_named_secret_and_failures_are_loud() {
    let fx = Fixture::new();
    let bws = fx.stub("bws", "BWS_ACCESS_TOKEN", LISTING);
    // A token in the launch environment is not the configured secret.
    let sources = ExternalSources::with_programs(None, Some((bitwarden(0), bws.clone())));
    let handle = fx.handle(sources, &[("BWS_ACCESS_TOKEN", "good")]);
    let err = handle.resolve("BULK_ONLY").unwrap_err();
    assert_eq!(err.code().code, "secret.source_failed");
    assert!(
        err.message().contains("BWS_ACCESS_TOKEN is not set"),
        "{}",
        err.message()
    );
    assert_eq!(fx.runs(), 0);

    let mut config = bitwarden(0);
    config.access_token_secret = SecretName("BW_MACHINE".to_owned());
    fx.store
        .set("BW_MACHINE", &SecretString::new("good"))
        .unwrap();
    let sources = ExternalSources::with_programs(None, Some((config, bws)));
    let handle = fx.handle(sources, &[]);
    assert_eq!(handle.resolve("BULK_ONLY").unwrap().expose(), "b1");
    // The token is revoked: no cached value is used instead.
    fx.store
        .set("BW_MACHINE", &SecretString::new("revoked"))
        .unwrap();
    let err = handle.resolve("BULK_ONLY").unwrap_err();
    assert_eq!(err.code().code, "secret.source_failed");
    assert!(
        err.message().contains("access token may be invalid"),
        "{}",
        err.message()
    );
}
