//! The daemon side of the web dashboard (`pmt_web::DashboardHost`): served
//! profiles' scopes and stores, their catalogs and forced redaction, the
//! same views the API server gets.

use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_gateway::HostFuture;
use pmt_store::Store;
use serde_json::Value;

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
            };
            super::api_profile::catalog(self, &profile, what)
        })
    }
}
