//! The shell's side of the connection registry: the requests the page
//! sends about connections, the errors latched until the user acts (a
//! changed certificate or host key is never retried), and the one-use
//! tickets of the loopback forwarders.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use pmt_config::secrets::keychain::Keychain;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::credentials::{Credentials, Pins};
use super::ssh::{Ssh, SshPrograms};
use super::{Connection, Kind, LOCAL, Registry};
use crate::dial::{self, DialError};
use crate::request::Request;
use crate::shell::{Shell, ok, to_json};

/// `bytes` random bytes as lowercase hex.
///
/// # Panics
///
/// When the OS has no random generator.
#[must_use]
pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0_u8; bytes];
    getrandom::fill(&mut buf).expect("the OS random generator is available");
    hex(&buf)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

fn hash(text: &str) -> String {
    hex(&Sha256::digest(text.as_bytes()))
}

/// Where the shell keeps connections.
#[derive(Debug, Clone)]
pub struct ConnectionsConfig {
    /// The app's data directory (registry, pins, `known_hosts`).
    pub dir: PathBuf,
    /// Where device credentials live (the OS keychain in the app).
    pub keychain: Arc<dyn Keychain>,
    /// The `ssh` programs.
    pub ssh: SshPrograms,
}

/// An error kept until the user acts on it: the shell stops dialing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Latch {
    /// A pinned server presented another key (Re-pin clears it).
    Certificate {
        /// The pin.
        pinned: String,
        /// The presented key.
        presented: String,
        /// What to show.
        message: String,
    },
    /// An SSH host presented another host key (Trust clears it).
    HostKey {
        /// The new key's fingerprint.
        fingerprint: String,
        /// What to show.
        message: String,
    },
}

impl Latch {
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Certificate { message, .. } | Self::HostKey { message, .. } => message,
        }
    }
}

/// What forwarders and dials share.
#[derive(Debug)]
pub(crate) struct Shared {
    pub(crate) credentials: Option<Credentials>,
    pub(crate) pins: Pins,
    latches: Mutex<HashMap<String, Latch>>,
    tickets: Mutex<HashMap<String, (String, Instant)>>,
}

impl Shared {
    pub(crate) fn latch(&self, id: &str, latch: Latch) {
        tracing::warn!(
            connection = id,
            message = latch.message(),
            "connection latched"
        );
        self.latches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.to_owned(), latch);
    }

    pub(crate) fn latched(&self, id: &str) -> Option<Latch> {
        self.latches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    fn unlatch(&self, id: &str) {
        self.latches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
    }

    fn all_latches(&self) -> HashMap<String, Latch> {
        self.latches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// A one-use ticket of connection `id`'s forwarder, under the same
    /// limits as the shell's own dial tickets ([`dial::TTL`],
    /// [`dial::MAX_PENDING`]).
    pub(crate) fn issue(&self, id: &str) -> Result<String, DialError> {
        let mut tickets = self.tickets.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        tickets.retain(|_, (_, expires)| *expires > now);
        if tickets.len() >= dial::MAX_PENDING {
            return Err(DialError::TooMany);
        }
        let ticket = random_hex(24);
        tickets.insert(hash(&ticket), (id.to_owned(), now + dial::TTL));
        Ok(ticket)
    }

    /// Whether `ticket` is a live one of connection `id` (it is used now).
    pub(crate) fn redeem(&self, id: &str, ticket: &str) -> bool {
        let mut tickets = self.tickets.lock().unwrap_or_else(PoisonError::into_inner);
        match tickets.remove(&hash(ticket)) {
            Some((owner, expires)) => owner == id && expires > Instant::now(),
            None => false,
        }
    }
}

/// The shell's connections.
pub(crate) struct Connections {
    pub(crate) registry: Mutex<Registry>,
    pub(crate) shared: Arc<Shared>,
    pub(crate) ssh: Option<Ssh>,
    pub(crate) live: tokio::sync::Mutex<HashMap<String, super::dialing::Live>>,
}

impl Default for Connections {
    /// The local connection only: no data directory, no keychain.
    fn default() -> Self {
        Self::build(Registry::default(), None, None)
    }
}

impl Connections {
    pub(crate) fn new(config: ConnectionsConfig) -> Self {
        let registry = Registry::open(&config.dir);
        let ssh = Ssh::new(config.ssh, &config.dir.join("ssh"));
        Self::build(registry, Some(Credentials::new(config.keychain)), Some(ssh))
    }

    fn build(registry: Registry, credentials: Option<Credentials>, ssh: Option<Ssh>) -> Self {
        let pins = Pins::at(registry.dir());
        Self {
            registry: Mutex::new(registry),
            shared: Arc::new(Shared {
                credentials,
                pins,
                latches: Mutex::default(),
                tickets: Mutex::default(),
            }),
            ssh,
            live: tokio::sync::Mutex::default(),
        }
    }

    pub(crate) fn get(&self, id: &str) -> Result<Connection, String> {
        self.registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
            .ok_or_else(|| format!("no connection {id}"))
    }

    pub(crate) fn with<T>(&self, f: impl FnOnce(&mut Registry) -> T) -> T {
        f(&mut self.registry.lock().unwrap_or_else(PoisonError::into_inner))
    }

    pub(crate) fn ssh(&self) -> Result<&Ssh, String> {
        self.ssh
            .as_ref()
            .ok_or_else(|| "this shell has no ssh connections".to_owned())
    }

    pub(crate) fn credentials(&self) -> Result<&Credentials, String> {
        self.shared
            .credentials
            .as_ref()
            .ok_or_else(|| "this shell keeps no credentials".to_owned())
    }
}

/// Whether `request` is about connections.
pub(crate) fn owns(request: &Request) -> bool {
    matches!(
        request,
        Request::ConnectionsList
            | Request::ConnectionSave(_)
            | Request::ConnectionRemove { .. }
            | Request::ConnectionPrimary { .. }
            | Request::ConnectionOnLaunch { .. }
            | Request::ConnectionConnected { .. }
            | Request::ConnectionTest { .. }
            | Request::ConnectionTrust { .. }
            | Request::ConnectionPair { .. }
            | Request::ConnectionWarm { .. }
            | Request::AgentsRoster
            | Request::PluginConnections
            | Request::UpdateAll
    )
}

impl Shell {
    /// The registry's setup (the app's data directory and keychain).
    #[must_use]
    pub fn with_connections(mut self, config: ConnectionsConfig) -> Self {
        self.connections = Connections::new(config);
        self
    }

    pub(crate) async fn connection_request(&self, request: Request) -> Result<Value, String> {
        let c = &self.connections;
        match request {
            Request::ConnectionsList => Ok(self.connections_list()),
            Request::ConnectionSave(draft) => to_json(&c.with(|r| r.save(&draft))?),
            Request::ConnectionRemove { id } => self.remove_connection(&id).await,
            Request::ConnectionPrimary { id } => c.with(|r| r.set_primary(&id)).map(|()| ok()),
            Request::ConnectionOnLaunch { mode } => {
                c.with(|r| r.set_on_launch(mode)).map(|()| ok())
            }
            Request::ConnectionConnected { id } => c.with(|r| r.connected(&id)).map(|()| ok()),
            Request::ConnectionTest { id } => to_json(&self.test_connection(&id).await?),
            Request::ConnectionTrust { id, fingerprint } => {
                self.trust(&id, &fingerprint).await.map(|()| ok())
            }
            Request::ConnectionPair { id, code, token } => {
                self.pair(&id, code.as_deref(), token.as_deref()).await
            }
            Request::ConnectionWarm { id } => self.warm(&id).await.map(|()| ok()),
            Request::AgentsRoster => to_json(&self.roster().await),
            Request::PluginConnections => Ok(c.with(|r| r.for_plugins())),
            _ => to_json(&self.update_all().await),
        }
    }

    /// The registry, which connections are latched and which have a
    /// stored credential (never the credential itself).
    fn connections_list(&self) -> Value {
        let c = &self.connections;
        let listing = c.with(|r| r.listing());
        let paired: Vec<&str> = listing
            .connections
            .iter()
            .filter(|conn| conn.kind == Kind::Remote)
            .filter(|conn| {
                c.shared
                    .credentials
                    .as_ref()
                    .is_some_and(|k| k.token(&conn.id).ok().flatten().is_some())
            })
            .map(|conn| conn.id.as_str())
            .collect();
        let mut value = json!(listing);
        value["latched"] = json!(c.shared.all_latches());
        value["paired"] = json!(paired);
        value
    }

    async fn remove_connection(&self, id: &str) -> Result<Value, String> {
        let c = &self.connections;
        c.with(|r| r.remove(id))?;
        c.live.lock().await.remove(id);
        c.shared.unlatch(id);
        let _ = c.shared.pins.set(id, None);
        if let Some(credentials) = &c.shared.credentials {
            credentials.forget(id);
        }
        Ok(ok())
    }

    /// The user compared and confirmed `fingerprint`: the certificate pin
    /// of a `remote` connection, the host key of an `ssh` one. Clears a
    /// latched error.
    async fn trust(&self, id: &str, fingerprint: &str) -> Result<(), String> {
        let c = &self.connections;
        let connection = c.get(id)?;
        match connection.kind {
            Kind::Remote => c.shared.pins.set(id, Some(fingerprint))?,
            Kind::Ssh => {
                let target = connection.ssh.as_ref().ok_or("no ssh target")?;
                c.ssh()?.trust(target, fingerprint).await?;
            }
            Kind::Local => return Err("the local connection needs no trust".to_owned()),
        }
        c.shared.unlatch(id);
        // A forwarder of the old key is gone with it.
        c.live.lock().await.remove(id);
        Ok(())
    }

    /// A device credential for a `remote` connection: a pairing code
    /// (`prometheus dashboard pair`) exchanged on the server, or the
    /// server's deployment token (`dashboard.auth.mode = "token"`). It goes
    /// straight to the keychain.
    async fn pair(
        &self,
        id: &str,
        code: Option<&str>,
        token: Option<&str>,
    ) -> Result<Value, String> {
        let c = &self.connections;
        let connection = c.get(id)?;
        if connection.kind != Kind::Remote {
            return Err("only remote connections are paired".to_owned());
        }
        let credentials = c.credentials()?;
        let token = match (code, token) {
            (_, Some(token)) if !token.trim().is_empty() => token.trim().to_owned(),
            (Some(code), _) => self.exchange_code(&connection, code.trim()).await?,
            _ => return Err("enter a pairing code or a token".to_owned()),
        };
        credentials.set_token(id, &token)?;
        Ok(ok())
    }

    pub(crate) fn is_local(id: Option<&str>) -> bool {
        id.is_none_or(|id| id == LOCAL)
    }
}

#[cfg(test)]
mod tests {
    use super::Connections;
    use crate::dial::{DialError, MAX_PENDING};

    /// Scenario: Ticket flood (the forwarders' tickets): the shell's dial
    /// limit applies to them too, with the same refusal.
    #[test]
    fn forwarder_tickets_share_the_dial_limit() {
        let connections = Connections::default();
        for _ in 0..MAX_PENDING {
            connections.shared.issue("r1").unwrap();
        }
        let refused = connections.shared.issue("r1");
        assert_eq!(refused, Err(DialError::TooMany));
        assert_eq!(
            refused.unwrap_err().to_string(),
            "Too many pending gateway dials"
        );
    }

    #[test]
    fn a_forwarder_ticket_works_once_and_only_for_its_connection() {
        let connections = Connections::default();
        let ticket = connections.shared.issue("r1").unwrap();
        assert!(!connections.shared.redeem("r2", &ticket));
        let again = connections.shared.issue("r1").unwrap();
        assert!(connections.shared.redeem("r1", &again));
        assert!(!connections.shared.redeem("r1", &again), "one use");
    }
}
