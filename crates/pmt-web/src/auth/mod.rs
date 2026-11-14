//! Who may use the dashboard (spec: web-dashboard, Loopback authentication
//! with an `HttpOnly` session cookie; Non-loopback binds fail closed; Remote
//! dashboard users are observers by default).
//!
//! The owner signs in with one-time login links minted over the control
//! channel; such sessions are drivers. Remote users sign in through
//! `dashboard.auth.mode` (`password`, `oidc` or `portal`) and are
//! observers unless their subject is in `dashboard.auth.drivers`. Every
//! signed-in browser holds only an `HttpOnly` cookie; its CSRF token is
//! read from `/api/session` by the SPA, never written into served HTML.
//! Native clients present a bearer token instead (spec: web-dashboard,
//! Additional dashboard sign-in providers; Paired device credentials for
//! remote clients): the token mode's deployment credential or a paired
//! device's token.

pub(crate) mod oidc;
pub(crate) mod password;
pub(crate) mod secret;
pub(crate) mod sessions;
pub(crate) mod tickets;

use std::sync::Arc;

use pmt_config::daemon_dashboard::{AuthMode, DashboardConfig};
use pmt_gateway::devices::{DeviceRole, Devices};
use tokio_util::sync::CancellationToken;

use crate::http::Req;
use sessions::{Role, Signed, Via};

/// The subject of a session opened with a login link.
pub(crate) const LOCAL: &str = "local";

/// The listener's sign-in state.
#[derive(Debug)]
pub(crate) struct Auth {
    pub tickets: tickets::Tickets,
    pub sessions: sessions::Sessions,
    pub limiter: password::Limiter,
    /// The OIDC sign-in (`oidc` mode) or the portal's (`portal` mode).
    pub oidc: Option<oidc::Oidc>,
    pub mode: AuthMode,
    pub devices: Arc<Devices>,
    drivers: Vec<String>,
    /// Ends the token mode's sockets when the listener's credential goes.
    deployment_ended: CancellationToken,
}

impl Auth {
    pub(crate) fn new(config: &DashboardConfig, devices: Arc<Devices>) -> Self {
        let oidc = match config.auth.mode {
            AuthMode::Oidc => oidc::Oidc::new(&config.auth.oidc),
            AuthMode::Portal => oidc::Oidc::portal(&config.auth.portal),
            _ => None,
        };
        Self {
            tickets: tickets::Tickets::default(),
            sessions: sessions::Sessions::default(),
            limiter: password::Limiter::default(),
            oidc,
            mode: config.auth.mode,
            devices,
            drivers: config.auth.drivers.clone(),
            deployment_ended: CancellationToken::new(),
        }
    }

    /// The role of a newly signed-in `subject`: the owner's login links
    /// drive; remote users observe unless listed as drivers.
    pub(crate) fn role_for(&self, subject: &str) -> Role {
        if subject == LOCAL || self.drivers.iter().any(|d| d == subject) {
            Role::Driver
        } else {
            Role::Observer
        }
    }

    /// The signed-in browser of `req` with its cookie token.
    pub(crate) fn signed(&self, req: &Req) -> Option<(String, Signed)> {
        let token = sessions::token_of(req.header("cookie")?)?;
        let signed = self.sessions.get(token)?;
        Some((token.to_owned(), signed))
    }

    /// The caller of `req`: a bearer token (a paired device's, or the
    /// token mode's deployment credential `deployment` compared in
    /// constant time), else the browser's cookie. The cookie token is
    /// `None` for bearer callers.
    pub(crate) fn caller(
        &self,
        req: &Req,
        deployment: impl FnOnce() -> Option<String>,
    ) -> Option<(Option<String>, Signed)> {
        let Some(bearer) = bearer_of(req) else {
            return self
                .signed(req)
                .map(|(token, signed)| (Some(token), signed));
        };
        if let Some(device) = self.devices.verify(bearer) {
            let role = match device.role {
                DeviceRole::Driver | DeviceRole::Admin => Role::Driver,
                DeviceRole::Observer => Role::Observer,
            };
            let signed = Signed {
                subject: format!("device:{}", device.name),
                role,
                csrf: String::new(),
                via: Via::Device,
                profiles: (!device.profiles.is_empty()).then_some(device.profiles),
                admin: device.role == DeviceRole::Admin,
                ended: device.revoked,
            };
            return Some((None, signed));
        }
        let expected = (self.mode == AuthMode::Token).then(deployment).flatten()?;
        secret::same(&secret::hash(bearer), &secret::hash(&expected)).then(|| {
            let signed = Signed {
                subject: "token".to_owned(),
                role: self.role_for("token"),
                csrf: String::new(),
                via: Via::Deployment,
                profiles: None,
                admin: false,
                ended: self.deployment_ended.clone(),
            };
            (None, signed)
        })
    }

    /// The sign-in providers `/api/health` reports.
    pub(crate) fn providers(&self) -> Vec<&'static str> {
        let mut providers = vec!["login_link"];
        if self.mode != AuthMode::None {
            providers.push(self.mode.as_str());
        }
        providers
    }
}

/// The token of an `Authorization: Bearer` header.
pub(crate) fn bearer_of(req: &Req) -> Option<&str> {
    let value = req.header("authorization")?;
    let (scheme, token) = value.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty()).then(|| token.trim())
}

#[cfg(test)]
mod tests {
    use pmt_config::daemon_dashboard::{AuthMode, DashboardConfig};

    use super::sessions::Role;
    use super::{Auth, LOCAL};

    #[test]
    fn remote_users_observe_unless_listed_as_drivers() {
        let mut config = DashboardConfig::default();
        config.auth.mode = AuthMode::Password;
        config.auth.drivers = vec!["alice".into()];
        let dir = tempfile::tempdir().unwrap();
        let devices = std::sync::Arc::new(pmt_gateway::devices::Devices::open(dir.path()));
        let auth = Auth::new(&config, devices);
        assert_eq!(auth.role_for(LOCAL), Role::Driver);
        assert_eq!(auth.role_for("alice"), Role::Driver);
        assert_eq!(auth.role_for("password"), Role::Observer);
        assert_eq!(auth.role_for("bob"), Role::Observer);
        assert_eq!(auth.providers(), ["login_link", "password"]);
    }
}
