//! What the ACP adapter takes from this binary (spec: acp-adapter): the
//! daemon connection bound to the launch profile, the profile's
//! credentials for the editor's sign-in, the egress-controlled image
//! fetcher, and the managed Chromium install of `--setup-browser`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_config::schema::ProviderAuth;
use pmt_config::{Overrides, ProfileScope};
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_providers::ProviderRegistry;
use pmt_rpc::contracts::{ClientCapabilities, ClientKind, HelloParams, Version};
use pmt_rpc::discovery::DaemonRecord;

use crate::Context;

/// The terminal auth method every editor is offered.
const SETUP_METHOD: &str = "prometheus-setup";

/// The largest remote image a prompt fetches.
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// The running daemon of `home`, started when `daemon.autostart` allows.
///
/// # Errors
///
/// `daemon.not_running` when none runs and autostart is off; start errors.
fn daemon_record(home: &Path, profile: &str) -> Result<DaemonRecord, Error> {
    if let Some(record) = crate::daemon_client::running(home)? {
        return Ok(record);
    }
    daemon_available(home)?;
    crate::daemon_cmd::running_or_start(home, Some(profile)).map(|(record, _)| record)
}

/// Whether the daemon of `home` runs or may be started (`--check`).
///
/// # Errors
///
/// `daemon.not_running` naming how to start it.
pub(super) fn daemon_available(home: &Path) -> Result<(), Error> {
    if crate::daemon_client::running(home)?.is_some()
        || pmt_config::DaemonConfig::load(home)?.daemon.autostart
    {
        return Ok(());
    }
    Err(Error::new(
        &codes::DAEMON_NOT_RUNNING,
        "gateway daemon is not running and daemon.autostart is off; start it with 'prometheus daemon start'",
    ))
}

/// Connects to the home's daemon as an `acp` client of the launch profile.
pub(super) struct DaemonConnector {
    pub(super) home: PathBuf,
    pub(super) profile: String,
    pub(super) unattended: bool,
}

impl pmt_acp::Connector for DaemonConnector {
    fn connect<'a>(&'a self, features: &'a [String]) -> pmt_acp::ConnectFuture<'a> {
        Box::pin(async move {
            let (home, profile) = (self.home.clone(), self.profile.clone());
            let record = pmt_core::spawn::blocking(move || daemon_record(&home, &profile))
                .await
                .map_err(|e| Error::new(&codes::DAEMON_START_FAILED, e.to_string()))??;
            let hello = HelloParams {
                client: ClientKind::Acp,
                client_version: crate::build_info::VERSION.to_owned(),
                protocol: Version::from(pmt_core::protocol::RPC_PROTOCOL),
                capabilities: ClientCapabilities {
                    // An unattended client is never asked anything.
                    server_requests: !self.unattended,
                    events: Vec::new(),
                    features: features.to_vec(),
                },
            };
            let (client, inbox, notice) =
                crate::daemon_client::connect(&record, &self.profile, &hello).await?;
            if let Some(notice) = notice {
                tracing::warn!("{notice}");
            }
            Ok((client, inbox))
        })
    }
}

/// The profile's credentials, re-read at each sign-in (the person may have
/// run `prometheus setup` meanwhile).
pub(super) struct ProfileAuth {
    profile: pmt_config::ProfileDir,
    set: Vec<String>,
    launch: LaunchEnv,
    program: String,
}

impl ProfileAuth {
    pub(super) fn new(ctx: &Context<'_>) -> Result<Self, Error> {
        Ok(Self {
            profile: ctx.profile_dir()?,
            set: ctx.set.to_vec(),
            launch: ctx.launch.clone(),
            program: std::env::current_exe().map_or_else(
                |_| "prometheus".to_owned(),
                |p| p.to_string_lossy().into_owned(),
            ),
        })
    }

    /// The default provider when its credential resolves now.
    fn provider(&self) -> Option<String> {
        let overrides = Overrides::parse(&self.set).ok()?;
        let scope = ProfileScope::open(
            self.profile.clone(),
            &overrides,
            pmt_config::Launch::Profile(&self.launch),
        )
        .ok()?;
        credential(&scope)
    }
}

/// The default provider of `scope` when its credential resolves.
fn credential(scope: &ProfileScope) -> Option<String> {
    let config = scope.config();
    let id = config.model.provider.as_deref().filter(|p| *p != "auto")?;
    let registry = ProviderRegistry::for_profile(config, Vec::new()).ok()?;
    let provider = registry.lookup(id).ok()?;
    let present = match provider.auth() {
        ProviderAuth::ApiKey => provider
            .secret_names()
            .iter()
            .any(|name| scope.secrets().resolve(name).is_ok()),
        // Sign-ins, minted tokens and credential chains resolve at the
        // provider call; local servers need none.
        _ => true,
    };
    present.then(|| id.to_owned())
}

impl pmt_acp::Auth for ProfileAuth {
    fn methods(&self) -> Vec<pmt_acp::AuthMethod> {
        let mut methods = vec![pmt_acp::AuthMethod {
            id: SETUP_METHOD.to_owned(),
            name: "Set up Prometheus".to_owned(),
            description: "Choose a model and sign in to its provider in a terminal".to_owned(),
            terminal: Some((self.program.clone(), vec!["setup".to_owned()])),
        }];
        if let Some(provider) = self.provider() {
            methods.push(pmt_acp::AuthMethod {
                description: format!("Use the profile's {provider} credential"),
                name: provider.clone(),
                id: provider,
                terminal: None,
            });
        }
        methods
    }

    fn resolves(&self, id: &str) -> bool {
        let provider = self.provider();
        id == SETUP_METHOD && provider.is_some() || provider.as_deref() == Some(id)
    }
}

/// Fetches remote prompt images under the profile's egress policy.
pub(super) struct EgressFetcher {
    fetcher: pmt_net::fetch::Fetcher,
}

impl EgressFetcher {
    pub(super) fn new(scope: &ProfileScope) -> Result<Self, Error> {
        let config = scope.config();
        let outbound = pmt_net::OutboundPolicy {
            force_ipv4: config.network.force_ipv4,
            extra_ca_file: config.network.extra_ca_file.as_ref().map(Into::into),
            proxy: config.network.proxy.clone(),
        };
        let egress = &config.security.egress;
        let hosts = crate::egress::host_policy(egress, &crate::egress::Overrides::default());
        let fetcher =
            pmt_net::fetch::Fetcher::new(&outbound, crate::egress::destinations(egress)?)?
                .with_hosts(Arc::new(hosts), "acp", None);
        Ok(Self { fetcher })
    }

    async fn get(&self, url: &str) -> Option<Vec<u8>> {
        let mut response = self.fetcher.get(url).await.ok()?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|n| n > MAX_IMAGE_BYTES as u64)
        {
            return None;
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if bytes.len() + chunk.len() > MAX_IMAGE_BYTES {
                return None;
            }
            bytes.extend_from_slice(&chunk);
        }
        Some(bytes)
    }
}

impl pmt_acp::Fetcher for EgressFetcher {
    fn fetch<'a>(
        &'a self,
        url: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = Option<Vec<u8>>> + Send + 'a>> {
        Box::pin(self.get(url))
    }
}

/// `--setup-browser`: installs the managed Chromium (asking first unless
/// `yes`); already installed is success.
///
/// # Errors
///
/// An invalid `daemon.toml`; install errors (an excluded runtime, a
/// download that fails).
pub(super) fn setup_browser(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    yes: bool,
) -> Result<u8, Error> {
    let home = scope.home();
    let store = pmt_update::runtimes::Store::open(home);
    if let Some(installed) = store.installed("chromium") {
        println!(
            "chromium is already installed at {}",
            installed.dir.display()
        );
        return Ok(0);
    }
    if !yes {
        if !crate::prompt::stdin_is_tty() {
            return Err(Error::new(
                &codes::CLI_USAGE,
                "installing Chromium needs a confirmation: run it in a terminal or pass --yes",
            ));
        }
        if !crate::prompt::confirm(
            "Download and install the Chromium runtime for ACP browser tools?",
        )? {
            println!("Chromium not installed");
            return Ok(0);
        }
    }
    let config = pmt_config::DaemonConfig::load(home)?;
    if config.runtimes.exclude.iter().any(|e| e == "chromium") {
        return Err(pmt_update::runtimes::install::excluded("chromium"));
    }
    let installer = crate::runtimes_cmd::installer(ctx, store, &config);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::new(&codes::OUTPUT_IO, format!("cannot start the runtime: {e}")))?;
    let done = runtime.block_on(installer.install("chromium"))?;
    println!(
        "chromium {} installed at {}",
        done.record.version,
        done.dir.display()
    );
    Ok(0)
}
