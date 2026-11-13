//! Who may use the dashboard (spec: web-dashboard, Loopback authentication
//! with an `HttpOnly` session cookie; Non-loopback binds fail closed; Remote
//! dashboard users are observers by default).
//!
//! The owner signs in with one-time login links minted over the control
//! channel; such sessions are drivers. Remote users sign in through
//! `dashboard.auth.mode` (`password` or `oidc` here) and are observers
//! unless their subject is in `dashboard.auth.drivers`. Every signed-in
//! browser holds only an `HttpOnly` cookie; its CSRF token is read from
//! `/api/session` by the SPA, never written into served HTML.

pub(crate) mod oidc;
pub(crate) mod password;
pub(crate) mod secret;
pub(crate) mod sessions;
pub(crate) mod tickets;

use pmt_config::daemon_dashboard::{AuthMode, DashboardConfig};

use crate::http::Req;
use sessions::{Role, Signed};

/// The subject of a session opened with a login link.
pub(crate) const LOCAL: &str = "local";

/// The listener's sign-in state.
#[derive(Debug)]
pub(crate) struct Auth {
    pub tickets: tickets::Tickets,
    pub sessions: sessions::Sessions,
    pub limiter: password::Limiter,
    pub oidc: Option<oidc::Oidc>,
    pub mode: AuthMode,
    drivers: Vec<String>,
}

impl Auth {
    pub(crate) fn new(config: &DashboardConfig) -> Self {
        let oidc = (config.auth.mode == AuthMode::Oidc)
            .then(|| oidc::Oidc::new(&config.auth.oidc))
            .flatten();
        Self {
            tickets: tickets::Tickets::default(),
            sessions: sessions::Sessions::default(),
            limiter: password::Limiter::default(),
            oidc,
            mode: config.auth.mode,
            drivers: config.auth.drivers.clone(),
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

    /// The sign-in providers `/api/health` reports.
    pub(crate) fn providers(&self) -> Vec<&'static str> {
        let mut providers = vec!["login_link"];
        if self.mode != AuthMode::None {
            providers.push(self.mode.as_str());
        }
        providers
    }
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
        let auth = Auth::new(&config);
        assert_eq!(auth.role_for(LOCAL), Role::Driver);
        assert_eq!(auth.role_for("alice"), Role::Driver);
        assert_eq!(auth.role_for("password"), Role::Observer);
        assert_eq!(auth.role_for("bob"), Role::Observer);
        assert_eq!(auth.providers(), ["login_link", "password"]);
    }
}
