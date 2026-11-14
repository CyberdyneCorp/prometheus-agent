//! Paired device credentials (spec: web-dashboard, Paired device
//! credentials for remote clients; messaging-gateway, Remote client
//! attachment): desktop apps and remote clients attach with a per-device
//! token bound to a device name, a role and the profiles it may use.
//!
//! Only SHA-256 hashes of the tokens are stored, in `<home>/run/devices.toml`
//! (0600, replaced atomically). A device gets its token either straight
//! from `prometheus daemon pair-device`, or by exchanging a one-use pairing
//! code (`prometheus dashboard pair`, valid 10 minutes, kept in the
//! daemon's memory only). Each refresh rotates the token; revoking a
//! device deletes its entry and ends its live sockets through the
//! cancellation token every attachment of the device watches.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// How long a pairing code may be exchanged.
pub const CODE_TTL: Duration = Duration::from_mins(10);
/// Pairing codes waiting at once; the oldest is dropped beyond it.
const MAX_CODES: usize = 64;
/// The registry file below the home.
pub const FILE: &str = "run/devices.toml";

/// What a device may do in sessions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceRole {
    /// Submits, interrupts and answers.
    #[default]
    Driver,
    /// Watches only.
    Observer,
    /// A driver that may also administer the daemon (`daemon.update`).
    Admin,
}

impl DeviceRole {
    /// Parses `driver`, `observer` or `admin`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "driver" => Some(Self::Driver),
            "observer" => Some(Self::Observer),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }

    /// The spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Driver => "driver",
            Self::Observer => "observer",
            Self::Admin => "admin",
        }
    }
}

/// One paired device as stored (no token, only its hash).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// Its role.
    #[serde(default)]
    pub role: DeviceRole,
    /// Profiles it may attach to (empty: every served profile).
    #[serde(default)]
    pub profiles: Vec<String>,
    /// The SHA-256 of its current token.
    pub token_sha256: String,
    /// When it was paired (RFC 3339).
    pub paired_at: String,
    /// When its token last rotated (RFC 3339).
    pub rotated_at: String,
}

/// A device as `devices` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceView {
    /// Its name.
    pub name: String,
    /// Its role.
    pub role: DeviceRole,
    /// Its profiles (empty: every served profile).
    pub profiles: Vec<String>,
    /// When it was paired.
    pub paired_at: String,
    /// When its token last rotated.
    pub rotated_at: String,
}

/// A device that presented a valid token.
#[derive(Debug, Clone)]
pub struct Verified {
    /// Its name.
    pub name: String,
    /// Its role.
    pub role: DeviceRole,
    /// Its profiles (empty: every served profile).
    pub profiles: Vec<String>,
    /// Cancelled when the device is revoked.
    pub revoked: CancellationToken,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    devices: BTreeMap<String, Device>,
}

#[derive(Debug)]
struct Code {
    name: String,
    role: DeviceRole,
    issued: Instant,
}

/// The device registry of one home.
#[derive(Debug)]
pub struct Devices {
    path: PathBuf,
    /// Serializes read-modify-write of the file within the process.
    write: Mutex<()>,
    live: Mutex<HashMap<String, CancellationToken>>,
    codes: Mutex<HashMap<String, Code>>,
}

fn sha256(token: &str) -> String {
    use std::fmt::Write as _;
    Sha256::digest(token.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

fn random(bytes: usize) -> String {
    let mut buf = vec![0_u8; bytes];
    getrandom::fill(&mut buf).expect("the OS random generator is available");
    buf.iter()
        .fold(String::with_capacity(bytes * 2), |mut out, b| {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// Compares two hashes in time independent of where they differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0_u8, |d, (x, y)| d | (x ^ y)) == 0
}

fn io_error(path: &Path, e: &dyn std::fmt::Display) -> Error {
    Error::new(&codes::HOME_IO, format!("{}: {e}", path.display()))
}

/// A device name: 1-64 letters, digits, `-`, `_` or `.`.
///
/// # Errors
///
/// `cli.usage` naming the rule.
pub fn check_name(name: &str) -> Result<(), Error> {
    let ok = (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    if ok {
        Ok(())
    } else {
        Err(Error::new(
            &codes::CLI_USAGE,
            format!("device name '{name}' must be 1-64 letters, digits, '-', '_' or '.'"),
        ))
    }
}

impl Devices {
    /// The registry of the home at `home`.
    #[must_use]
    pub fn open(home: &Path) -> Self {
        Self {
            path: home.join(FILE),
            write: Mutex::default(),
            live: Mutex::default(),
            codes: Mutex::default(),
        }
    }

    fn load(&self) -> Result<File, Error> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => toml::from_str(&text).map_err(|e| io_error(&self.path, &e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(File::default()),
            Err(e) => Err(io_error(&self.path, &e)),
        }
    }

    fn save(&self, file: &File) -> Result<(), Error> {
        let text = toml::to_string(file).map_err(|e| io_error(&self.path, &e))?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| io_error(dir, &e))?;
        }
        pmt_core::private_fs::write_atomic(&self.path, text.as_bytes())
            .map_err(|e| io_error(&self.path, &e))
    }

    fn edit<T>(&self, change: impl FnOnce(&mut File) -> Result<T, Error>) -> Result<T, Error> {
        let _guard = self.write.lock().unwrap_or_else(PoisonError::into_inner);
        let mut file = self.load()?;
        let out = change(&mut file)?;
        self.save(&file)?;
        Ok(out)
    }

    /// Every paired device.
    ///
    /// # Errors
    ///
    /// `io` when the registry cannot be read.
    pub fn list(&self) -> Result<Vec<DeviceView>, Error> {
        Ok(self
            .load()?
            .devices
            .into_iter()
            .map(|(name, d)| DeviceView {
                name,
                role: d.role,
                profiles: d.profiles,
                paired_at: d.paired_at,
                rotated_at: d.rotated_at,
            })
            .collect())
    }

    /// Pairs `name` (replacing a device of that name, whose sockets end)
    /// and returns its token, which is shown once and never stored.
    ///
    /// # Errors
    ///
    /// `cli.usage` for a bad name, `io`.
    pub fn issue(
        &self,
        name: &str,
        role: DeviceRole,
        profiles: &[String],
    ) -> Result<String, Error> {
        check_name(name)?;
        let token = format!("pmtd_{}", random(32));
        let now = jiff::Timestamp::now().to_string();
        let device = Device {
            role,
            profiles: profiles.to_vec(),
            token_sha256: sha256(&token),
            paired_at: now.clone(),
            rotated_at: now,
        };
        self.edit(|file| {
            file.devices.insert(name.to_owned(), device);
            Ok(())
        })?;
        self.end(name);
        Ok(token)
    }

    /// The device holding `token`.
    #[must_use]
    pub fn verify(&self, token: &str) -> Option<Verified> {
        let presented = sha256(token);
        let file = self.load().ok()?;
        let (name, device) = file
            .devices
            .into_iter()
            .find(|(_, d)| same(&d.token_sha256, &presented))?;
        let revoked = self
            .live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(name.clone())
            .or_default()
            .clone();
        Some(Verified {
            name,
            role: device.role,
            profiles: device.profiles,
            revoked,
        })
    }

    /// Rotates the token of the device holding `token`: the new token, the
    /// old one no longer works (`None`: not a device token).
    ///
    /// # Errors
    ///
    /// `io`.
    pub fn refresh(&self, token: &str) -> Result<Option<String>, Error> {
        let presented = sha256(token);
        let fresh = format!("pmtd_{}", random(32));
        self.edit(|file| {
            let Some(device) = file
                .devices
                .values_mut()
                .find(|d| same(&d.token_sha256, &presented))
            else {
                return Ok(None);
            };
            device.token_sha256 = sha256(&fresh);
            device.rotated_at = jiff::Timestamp::now().to_string();
            Ok(Some(fresh.clone()))
        })
    }

    /// Revokes `name`: its entry is deleted and its sockets end.
    ///
    /// # Errors
    ///
    /// `device.not_found`, `io`.
    pub fn revoke(&self, name: &str) -> Result<(), Error> {
        self.edit(|file| {
            file.devices.remove(name).map(drop).ok_or_else(|| {
                Error::new(
                    &codes::DEVICE_NOT_FOUND,
                    format!("no paired device '{name}'"),
                )
            })
        })?;
        self.end(name);
        Ok(())
    }

    /// Ends the live sockets of `name`.
    fn end(&self, name: &str) {
        if let Some(token) = self
            .live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(name)
        {
            token.cancel();
        }
    }

    /// A one-use pairing code for `name`, valid [`CODE_TTL`].
    ///
    /// # Errors
    ///
    /// `cli.usage` for a bad name.
    pub fn mint_code(&self, name: &str, role: DeviceRole) -> Result<String, Error> {
        check_name(name)?;
        let code = random(5).to_uppercase();
        let mut codes = self.codes.lock().unwrap_or_else(PoisonError::into_inner);
        codes.retain(|_, c| c.issued.elapsed() < CODE_TTL);
        if codes.len() >= MAX_CODES
            && let Some(oldest) = codes
                .iter()
                .min_by_key(|(_, c)| c.issued)
                .map(|(k, _)| k.clone())
        {
            codes.remove(&oldest);
        }
        codes.insert(
            sha256(&code),
            Code {
                name: name.to_owned(),
                role,
                issued: Instant::now(),
            },
        );
        Ok(code)
    }

    /// Exchanges a pairing code for a device token: the device name and
    /// its token (`None`: unknown, used or expired code).
    ///
    /// # Errors
    ///
    /// `io`.
    pub fn redeem_code(&self, code: &str) -> Result<Option<(String, String)>, Error> {
        let pending = self
            .codes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&sha256(&code.trim().to_uppercase()))
            .filter(|c| c.issued.elapsed() < CODE_TTL);
        let Some(pending) = pending else {
            return Ok(None);
        };
        let token = self.issue(&pending.name, pending.role, &[])?;
        Ok(Some((pending.name, token)))
    }
}

#[cfg(test)]
mod tests {
    use super::{CODE_TTL, DeviceRole, Devices};

    fn registry(name: &str) -> (tempfile::TempDir, Devices) {
        let dir = tempfile::Builder::new().prefix(name).tempdir().unwrap();
        let devices = Devices::open(dir.path());
        (dir, devices)
    }

    #[test]
    fn a_revoked_device_is_rejected_and_its_sockets_end() {
        let (dir, devices) = registry("revoke");
        let token = devices
            .issue("old-laptop", DeviceRole::Driver, &[])
            .unwrap();
        let verified = devices.verify(&token).expect("a fresh token verifies");
        assert_eq!(verified.name, "old-laptop");
        let stored = std::fs::read_to_string(dir.path().join("run/devices.toml")).unwrap();
        assert!(!stored.contains(&token), "only the hash is stored");
        devices.revoke("old-laptop").unwrap();
        assert!(verified.revoked.is_cancelled());
        assert!(devices.verify(&token).is_none());
        assert!(devices.revoke("old-laptop").is_err());
    }

    #[test]
    fn a_refresh_rotates_the_token() {
        let (_dir, devices) = registry("rotate");
        let first = devices.issue("desk", DeviceRole::Observer, &[]).unwrap();
        let second = devices.refresh(&first).unwrap().unwrap();
        assert!(devices.verify(&first).is_none());
        assert_eq!(devices.verify(&second).unwrap().role, DeviceRole::Observer);
        assert_eq!(devices.refresh(&first).unwrap(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_pairing_code_expires_after_ten_minutes_and_works_once() {
        let (_dir, devices) = registry("codes");
        let code = devices.mint_code("tablet", DeviceRole::Driver).unwrap();
        let (name, token) = devices.redeem_code(&code).unwrap().unwrap();
        assert_eq!(name, "tablet");
        assert!(devices.verify(&token).is_some());
        assert_eq!(devices.redeem_code(&code).unwrap(), None, "single use");

        let late = devices.mint_code("old-laptop", DeviceRole::Driver).unwrap();
        tokio::time::advance(CODE_TTL + std::time::Duration::from_mins(1)).await;
        assert_eq!(devices.redeem_code(&late).unwrap(), None);
        assert!(
            devices
                .list()
                .unwrap()
                .iter()
                .all(|d| d.name != "old-laptop")
        );
    }
}
