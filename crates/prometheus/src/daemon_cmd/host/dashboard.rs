//! The daemon side of the web dashboard (`pmt_web::DashboardHost`): served
//! profiles' scopes and stores, their catalogs and forced redaction, the
//! same views the API server gets.

use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_core::error::Error;
use pmt_cron::job::Job;
use pmt_gateway::HostFuture;
use pmt_store::Store;
use serde_json::{Value, json};

use super::DaemonHost;

impl pmt_web::DashboardHost for DaemonHost {
    fn scope(&self, profile: &str) -> Option<ProfileScope> {
        self.profile(profile).ok().map(|p| p.scope())
    }

    fn store(&self, profile: &str) -> Option<Arc<Store>> {
        self.profile(profile).ok().map(|p| Arc::clone(&p.store))
    }

    fn launch_profile(&self) -> String {
        self.launch_profile.clone()
    }

    /// The profile's known secrets and PII rules (every served profile's
    /// for daemon-wide logs), plus credential-shaped patterns.
    fn redact(&self, profile: Option<&str>, text: &str) -> String {
        let names = profile.map_or_else(|| self.names(), |p| vec![p.to_owned()]);
        names
            .iter()
            .filter_map(|name| self.profile(name).ok())
            .filter_map(|p| super::api::forced_redactor(&p.scope()).ok())
            .fold(text.to_owned(), |text, redactor| redactor.redact_all(&text))
    }

    fn catalog<'a>(&'a self, profile: &'a str, what: pmt_web::Catalog) -> HostFuture<'a, Value> {
        Box::pin(async move {
            let profile = self.profile(profile)?;
            let what = match what {
                pmt_web::Catalog::Skills => pmt_api::Catalog::Skills,
                pmt_web::Catalog::Toolsets => pmt_api::Catalog::Toolsets,
                pmt_web::Catalog::Models => pmt_api::Catalog::Models,
                pmt_web::Catalog::Plugins => return Ok(plugins(&profile.scope())),
                pmt_web::Catalog::Mcp => return Ok(crate::mcp_cmd::servers_json(&profile.scope())),
                pmt_web::Catalog::Cron => return cron(&profile.store),
                pmt_web::Catalog::Memory => return memory(&profile.store),
                pmt_web::Catalog::Webhooks => return crate::webhooks_cmd::rows(&profile.scope()),
            };
            super::api_profile::catalog(self, &profile, what)
        })
    }

    fn create_profile(&self, body: Value) -> HostFuture<'_, Value> {
        Box::pin(async move {
            let launch = self.profile(&self.launch_profile)?.scope();
            let home = pmt_config::Home::open(launch.home())?;
            super::profile_create::create(&home, &body)
        })
    }

    fn change<'a>(
        &'a self,
        profile: &'a str,
        change: pmt_web::Change,
        target: Option<&'a str>,
        body: Value,
    ) -> HostFuture<'a, Value> {
        Box::pin(async move {
            let served = self.profile(profile)?;
            let target = super::dashboard_change::Target {
                launch: &self.launch,
                scope: served.scope(),
                store: Arc::clone(&served.store),
                profile: served,
                key: target,
                body,
            };
            super::dashboard_change::apply(change, target).await
        })
    }
}

/// The profile's plugins as its sessions load them (from its workspace,
/// as the gateway's sessions run).
fn plugins(scope: &ProfileScope) -> Value {
    let cwd = scope.dir().join("workspace");
    let loaded = crate::plugins_host::for_session(scope, &cwd).current();
    json!({"plugins": crate::plugins_cmd::manage::rows(&loaded.statuses)})
}

/// Every cron job of the profile, completed ones included, as `cron list
/// --json --all` shows them.
fn cron(store: &Store) -> Result<Value, Error> {
    let jobs = store
        .cron_jobs()?
        .into_iter()
        .map(|row| Job::from_row(row).map(|job| crate::cron_host::tool::summary(&job)))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({"jobs": jobs}))
}

/// Every memory entry of the profile, as `memory list --all --json` shows
/// them.
fn memory(store: &Store) -> Result<Value, Error> {
    let entries: Vec<Value> = store
        .memory_entries(None)?
        .iter()
        .map(crate::memory_cmd::entry_json)
        .collect();
    Ok(json!({"entries": entries}))
}
