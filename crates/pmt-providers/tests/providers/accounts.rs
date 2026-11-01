//! Shared setup for the credential tests: a temporary profile with a file
//! secrets store and a real `state.db`, and [`Accounts`] over it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use pmt_config::schema::SecretsBackend;
use pmt_config::secrets::SecretStore;
use pmt_config::{Config, Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::error::Error;
use pmt_core::secret::SecretString;
use pmt_providers::auth::borrowed::Borrowed;
use pmt_providers::auth::refresh::Refresher;
use pmt_providers::auth::token::TokenSet;
use pmt_providers::auth::{Accounts, Credentials, Obtained};
use pmt_providers::resolve::{Resolver, Target, TcpProbe, Trace};
use pmt_providers::{ModelCaller, ProviderRegistry, Route};
use pmt_store::Store;

use crate::support::{caller, fast};

static PROBE: TcpProbe = TcpProbe {
    timeout: Duration::from_millis(200),
};

/// One profile: config, keys, sign-ins and `state.db`.
pub(crate) struct Setup {
    /// The directory holding the home (and the user home).
    pub root: PathBuf,
    _temp: Option<tempfile::TempDir>,
    pub profile_dir: PathBuf,
    pub config: Config,
    pub registry: ProviderRegistry,
    pub keys: BTreeMap<String, SecretString>,
    pub credentials: Credentials,
    pub store: Arc<Store>,
}

impl Setup {
    /// A profile in a fresh temporary directory.
    pub(crate) fn new(config: &str, keys: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let root = dir.path().to_path_buf();
        Self::at(&root, config, keys, Some(dir))
    }

    /// The profile another process created under `root` (two-process
    /// tests).
    pub(crate) fn reattach(root: &Path, config: &str) -> Self {
        Self::at(root, config, &[], None)
    }

    fn at(
        root: &Path,
        config: &str,
        keys: &[(&str, &str)],
        temp: Option<tempfile::TempDir>,
    ) -> Self {
        let home = Home::open(&root.join("home")).unwrap();
        let profile = home.profile(&ProfileName::default_profile()).unwrap();
        let profile_dir = profile.path().to_path_buf();
        let credentials = Credentials::new(SecretStore::open(&profile, SecretsBackend::File));
        let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap();
        let store = Arc::new(Store::open(&scope).unwrap());
        let config: Config = toml::from_str(config).unwrap();
        pmt_config::provider_urls::check(&config).unwrap();
        let registry = ProviderRegistry::for_profile(&config, Vec::new()).unwrap();
        let keys = keys
            .iter()
            .map(|(k, v)| ((*k).to_owned(), SecretString::new(*v)))
            .collect();
        Self {
            root: root.to_path_buf(),
            _temp: temp,
            profile_dir,
            config,
            registry,
            keys,
            credentials,
            store,
        }
    }

    /// `state.db` opened again, as a new process would.
    pub(crate) fn reopen_store(&self) -> Store {
        let home = Home::open(&self.root.join("home")).unwrap();
        let profile = home.profile(&ProfileName::default_profile()).unwrap();
        let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap();
        Store::open(&scope).unwrap()
    }

    /// The user home other agent CLIs keep their logins under.
    pub(crate) fn user_home(&self) -> PathBuf {
        let home = self.root.join("user");
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    /// Accounts with the pool state, a refresher and `borrowed`.
    pub(crate) fn accounts(&self, borrowed: Borrowed) -> Arc<Accounts> {
        let refresher = Refresher::new(http(), Refresher::lock_dir_of(&self.profile_dir));
        Arc::new(
            Accounts::new(
                self.registry.clone(),
                Arc::new(self.keys.clone()),
                self.credentials.clone(),
            )
            .with_state(self.store.clone())
            .with_refresher(refresher)
            .with_borrowed(borrowed),
        )
    }

    /// The route of `provider` (model `m`) resolved with `accounts`.
    pub(crate) async fn route(&self, accounts: &Accounts, provider: &str) -> Result<Route, Error> {
        let resolver = Resolver {
            registry: &self.registry,
            config: &self.config,
            secrets: &self.keys,
            extras: accounts,
            probe: &PROBE,
            ad_hoc: None,
        };
        let target = Target {
            provider: provider.to_owned(),
            model: Some("m".to_owned()),
            ..Target::default()
        };
        let mut route = resolver
            .resolve_target(&target, &mut Trace::default())
            .await?;
        fast(&mut route);
        Ok(route)
    }

    /// Stores sign-in `label` of `provider` expiring in `expires_in`.
    pub(crate) fn sign_in(
        &self,
        provider: &str,
        label: &str,
        access: &str,
        refresh: &str,
        expires_in: SignedDuration,
    ) {
        let tokens = TokenSet {
            access_token: SecretString::new(access),
            refresh_token: Some(SecretString::new(refresh)),
            expires_at: Some(Timestamp::now() + expires_in),
            id_token: None,
        };
        let obtained = Obtained {
            source: "device".to_owned(),
            ..Obtained::default()
        };
        self.credentials
            .save(provider, label, &tokens, &obtained)
            .unwrap();
    }

    /// Every file of the profile except the secrets store, as text.
    pub(crate) fn plain_files(&self) -> String {
        let mut text = String::new();
        collect(&self.root.join("home"), &mut text);
        text
    }
}

fn collect(dir: &Path, text: &mut String) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(&path, text);
        } else if path.file_name().is_some_and(|n| n != "secrets.toml") {
            text.push_str(&String::from_utf8_lossy(&std::fs::read(&path).unwrap()));
        }
    }
}

pub(crate) fn http() -> reqwest::Client {
    pmt_net::client(&pmt_net::OutboundPolicy::default()).unwrap()
}

/// A model caller running `accounts` around every request.
pub(crate) fn caller_with(accounts: &Arc<Accounts>) -> ModelCaller {
    caller().with_credentials(accounts.clone())
}

/// The bearer tokens the server received, in order.
pub(crate) async fn bearers(server: &wiremock::MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|r| r.headers.get("authorization"))
        .map(|v| v.to_str().unwrap().trim_start_matches("Bearer ").to_owned())
        .collect()
}
