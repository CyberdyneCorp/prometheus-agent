//! The connection registry (spec: desktop-app, Connection registry): the
//! daemons a desktop reaches. Exactly one `local` connection (the
//! app-managed daemon, never removable), any number of `remote` ones (the
//! dashboard listener of another daemon, over https) and `ssh` ones (a
//! daemon on another host reached through the system `ssh` client), each
//! with a unique case-insensitive name. The registry file holds names,
//! kinds, URLs and hosts only: device credentials live in the OS keychain
//! ([`credentials`]), certificate pins in `pins.json` and SSH host keys in
//! the app's own `known_hosts`, and none of them reaches the webview.

pub mod credentials;
mod dialing;
pub mod forward;
pub mod ops;
pub mod roster;
pub mod service;
pub mod ssh;
pub mod tls;
pub mod web;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

/// The id of the app-managed connection.
pub const LOCAL: &str = "local";
/// The longest connection name.
pub const MAX_NAME: usize = 64;
/// The registry file in the app's data directory.
pub const FILE: &str = "connections.json";
/// The refusal of a plain `http` URL to another machine.
pub const HTTPS_REQUIRED: &str = "remote connections require https (or an ssh connection)";

/// What a connection reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The daemon this app manages on this machine.
    Local,
    /// Another daemon's dashboard listener.
    Remote,
    /// A daemon on another host, through `ssh`.
    Ssh,
}

/// Where an `ssh` connection goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshTarget {
    /// The remote user (the `ssh` default when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// The host name or address.
    pub host: String,
    /// The SSH port.
    #[serde(default = "default_port")]
    pub port: u16,
    /// The remote profile the daemon is ensured for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// The remote `prometheus` binary when it is not on the
    /// non-interactive `PATH`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
}

fn default_port() -> u16 {
    22
}

impl SshTarget {
    /// `user@host:port` (`host:port` without a user).
    #[must_use]
    pub fn address(&self) -> String {
        match &self.user {
            Some(user) => format!("{user}@{}:{}", self.host, self.port),
            None => format!("{}:{}", self.host, self.port),
        }
    }

    /// What two `ssh` connections may not share: the address and the
    /// remote profile.
    fn identity(&self) -> String {
        format!(
            "{}/{}",
            self.address().to_ascii_lowercase(),
            self.profile.as_deref().unwrap_or("default")
        )
    }
}

/// One registered connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    /// Its stable id (`local` for the app-managed one).
    pub id: String,
    /// Its display name.
    pub name: String,
    /// What it reaches.
    pub kind: Kind,
    /// A `remote` connection's normalized URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// An `ssh` connection's target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshTarget>,
}

/// What the app opens at launch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnLaunch {
    /// The primary connection.
    #[default]
    Primary,
    /// The last connection that connected successfully.
    LastUsed,
}

/// A connection the page asks to save (`id` absent: a new one).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Draft {
    /// The connection to change.
    #[serde(default)]
    pub id: Option<String>,
    /// Its name.
    pub name: String,
    /// What it reaches (ignored for the local connection).
    pub kind: Kind,
    /// A `remote` connection's URL.
    #[serde(default)]
    pub url: Option<String>,
    /// An `ssh` connection's target.
    #[serde(default)]
    pub ssh: Option<SshTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stored {
    connections: Vec<Connection>,
    primary: String,
    #[serde(default)]
    open_on_launch: OnLaunch,
    #[serde(default)]
    last_used: Option<String>,
}

impl Default for Stored {
    fn default() -> Self {
        Self {
            connections: vec![Connection {
                id: LOCAL.to_owned(),
                name: "This computer".to_owned(),
                kind: Kind::Local,
                url: None,
                ssh: None,
            }],
            primary: LOCAL.to_owned(),
            open_on_launch: OnLaunch::Primary,
            last_used: None,
        }
    }
}

/// The registry, saved after every change when it has a directory.
#[derive(Debug, Default)]
pub struct Registry {
    dir: Option<PathBuf>,
    stored: Stored,
}

/// The registry as the page lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Listing {
    /// Every connection, `local` first.
    pub connections: Vec<Connection>,
    /// The primary connection's id.
    pub primary: String,
    /// What opens at launch.
    pub open_on_launch: OnLaunch,
    /// The connection to open at launch.
    pub launch: String,
}

impl Registry {
    /// The registry kept in `dir` (created on the first change); an
    /// unreadable file starts over with the local connection only.
    #[must_use]
    pub fn open(dir: &Path) -> Self {
        let stored = std::fs::read(dir.join(FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Stored>(&bytes).ok())
            .filter(|s| s.connections.iter().any(|c| c.kind == Kind::Local))
            .unwrap_or_default();
        Self {
            dir: Some(dir.to_path_buf()),
            stored,
        }
    }

    /// The directory the registry and its side files live in.
    #[must_use]
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// The registry as the page lists it.
    #[must_use]
    pub fn listing(&self) -> Listing {
        Listing {
            connections: self.stored.connections.clone(),
            primary: self.stored.primary.clone(),
            open_on_launch: self.stored.open_on_launch,
            launch: self.launch(),
        }
    }

    /// What a plugin may see (`host.connections()`): ids, names, kinds
    /// and the primary id, never a URL, host or credential.
    #[must_use]
    pub fn for_plugins(&self) -> Value {
        let rows: Vec<Value> = self
            .stored
            .connections
            .iter()
            .map(|c| json!({"id": c.id, "name": c.name, "kind": c.kind}))
            .collect();
        json!({"connections": rows, "primary": self.stored.primary})
    }

    /// The connection `id`.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Connection> {
        self.stored.connections.iter().find(|c| c.id == id)
    }

    /// Adds or changes a connection; the saved one.
    ///
    /// # Errors
    ///
    /// Why the draft is refused (a duplicate, plain http to another
    /// machine, a bad name), or the file could not be written.
    pub fn save(&mut self, draft: &Draft) -> Result<Connection, String> {
        let name = check_name(&draft.name)?;
        let existing = match &draft.id {
            Some(id) => Some(self.get(id).ok_or("no such connection")?.clone()),
            None => None,
        };
        let id = existing.as_ref().map_or_else(
            || crate::connections::service::random_hex(6),
            |c| c.id.clone(),
        );
        let connection = match existing {
            // The local connection only takes a new name.
            Some(local) if local.kind == Kind::Local => Connection { name, ..local },
            _ => build(id.clone(), name, draft)?,
        };
        self.refuse_duplicates(&connection)?;
        match self.stored.connections.iter_mut().find(|c| c.id == id) {
            Some(slot) => *slot = connection.clone(),
            None => self.stored.connections.push(connection.clone()),
        }
        self.write()?;
        Ok(connection)
    }

    fn refuse_duplicates(&self, new: &Connection) -> Result<(), String> {
        for other in self.stored.connections.iter().filter(|c| c.id != new.id) {
            if other.name.eq_ignore_ascii_case(&new.name) {
                return Err(format!("a connection named {} already exists", other.name));
            }
            if let (Some(a), Some(b)) = (&new.url, &other.url)
                && a == b
            {
                return Err(format!(
                    "a connection to {a} already exists ({})",
                    other.name
                ));
            }
            if let (Some(a), Some(b)) = (&new.ssh, &other.ssh)
                && a.identity() == b.identity()
            {
                return Err(format!(
                    "a connection to {} already exists ({})",
                    a.address(),
                    other.name
                ));
            }
        }
        Ok(())
    }

    /// Removes a connection; removing the primary makes `local` primary.
    ///
    /// # Errors
    ///
    /// The local connection, an unknown id, or the file could not be
    /// written.
    pub fn remove(&mut self, id: &str) -> Result<(), String> {
        if id == LOCAL {
            return Err("the local connection cannot be removed".to_owned());
        }
        let before = self.stored.connections.len();
        self.stored.connections.retain(|c| c.id != id);
        if self.stored.connections.len() == before {
            return Err("no such connection".to_owned());
        }
        if self.stored.primary == id {
            LOCAL.clone_into(&mut self.stored.primary);
        }
        if self.stored.last_used.as_deref() == Some(id) {
            self.stored.last_used = None;
        }
        self.write()
    }

    /// Marks `id` primary.
    ///
    /// # Errors
    ///
    /// An unknown id, or the file could not be written.
    pub fn set_primary(&mut self, id: &str) -> Result<(), String> {
        self.get(id).ok_or("no such connection")?;
        id.clone_into(&mut self.stored.primary);
        self.write()
    }

    /// Chooses what opens at launch.
    ///
    /// # Errors
    ///
    /// The file could not be written.
    pub fn set_on_launch(&mut self, mode: OnLaunch) -> Result<(), String> {
        self.stored.open_on_launch = mode;
        self.write()
    }

    /// `id` connected successfully: it is the last used one. A failed
    /// switch never calls this, so it is never remembered.
    ///
    /// # Errors
    ///
    /// The file could not be written.
    pub fn connected(&mut self, id: &str) -> Result<(), String> {
        self.get(id).ok_or("no such connection")?;
        if self.stored.last_used.as_deref() == Some(id) {
            return Ok(());
        }
        self.stored.last_used = Some(id.to_owned());
        self.write()
    }

    /// The connection to open at launch.
    #[must_use]
    pub fn launch(&self) -> String {
        let last = match self.stored.open_on_launch {
            OnLaunch::LastUsed => self.stored.last_used.as_deref(),
            OnLaunch::Primary => None,
        };
        last.filter(|id| self.get(id).is_some())
            .unwrap_or(&self.stored.primary)
            .to_owned()
    }

    fn write(&self) -> Result<(), String> {
        let Some(dir) = &self.dir else {
            return Ok(());
        };
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot save the connections: {e}"))?;
        let bytes = serde_json::to_vec_pretty(&self.stored).map_err(|e| e.to_string())?;
        pmt_core::private_fs::write_atomic(&dir.join(FILE), &bytes)
            .map_err(|e| format!("cannot save the connections: {e}"))
    }
}

fn check_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("a connection needs a name".to_owned());
    }
    if name.chars().count() > MAX_NAME {
        return Err(format!(
            "a connection name has at most {MAX_NAME} characters"
        ));
    }
    Ok(name.to_owned())
}

fn build(id: String, name: String, draft: &Draft) -> Result<Connection, String> {
    match draft.kind {
        Kind::Local => Err("there is only one local connection".to_owned()),
        Kind::Remote => {
            let url = normalize(
                draft
                    .url
                    .as_deref()
                    .ok_or("a remote connection needs a URL")?,
            )?;
            Ok(Connection {
                id,
                name,
                kind: Kind::Remote,
                url: Some(url),
                ssh: None,
            })
        }
        Kind::Ssh => {
            let mut target = draft.ssh.clone().ok_or("an ssh connection needs a host")?;
            target.host = target.host.trim().to_owned();
            if target.host.is_empty() || target.host.starts_with('-') {
                return Err("an ssh connection needs a host".to_owned());
            }
            if target.user.as_deref().is_some_and(|u| u.starts_with('-')) {
                return Err("not a user name".to_owned());
            }
            Ok(Connection {
                id,
                name,
                kind: Kind::Ssh,
                url: None,
                ssh: Some(target),
            })
        }
    }
}

/// A remote URL as the registry keeps it: trimmed, without trailing
/// slashes, lowercased; `https`, or `http` to a loopback host only.
///
/// # Errors
///
/// Not a URL, another scheme, credentials, a query or a fragment, or
/// [`HTTPS_REQUIRED`].
pub fn normalize(raw: &str) -> Result<String, String> {
    let text = raw.trim().trim_end_matches('/').to_ascii_lowercase();
    let url = Url::parse(&text).map_err(|_| format!("not a URL: {}", raw.trim()))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("put credentials in the pairing step, not in the URL".to_owned());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("a connection URL has no query or fragment".to_owned());
    }
    let host = url.host().ok_or("the URL needs a host")?;
    match url.scheme() {
        "https" => Ok(text),
        "http" if is_loopback(&host) => Ok(text),
        "http" => Err(HTTPS_REQUIRED.to_owned()),
        _ => Err("a remote connection URL starts with https://".to_owned()),
    }
}

fn is_loopback(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Domain(name) => name.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(ip) => ip.is_loopback(),
        url::Host::Ipv6(ip) => ip.is_loopback(),
    }
}

#[cfg(test)]
mod tests;
