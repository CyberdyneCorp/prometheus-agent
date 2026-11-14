//! Paired devices from the command line (spec: messaging-gateway, Remote
//! client attachment; web-dashboard, Paired device credentials for remote
//! clients): `daemon pair-device` issues a device token directly (the
//! registry is a file of hashes, so no daemon is needed), and revoking
//! goes through the running daemon's control channel (answered by the
//! gateway itself, so a daemon without the dashboard revokes too), which
//! also closes the device's open sockets with 4401, or edits the registry
//! when no daemon runs.

use std::path::Path;
use std::time::Duration;

use pmt_core::error::{Error, codes};
use pmt_gateway::devices::{DeviceRole, Devices};
use pmt_rpc::control::ControlRequest;
use serde_json::json;

use super::{control_call, send_cmd::control_error, wait_running};
use crate::output::{document, print_raw};

/// `prometheus daemon pair-device` arguments.
#[derive(Debug, Clone, clap::Args)]
pub struct PairDeviceArgs {
    /// The device's name (letters, digits, `-`, `_`, `.`).
    pub name: String,
    /// A profile the device may attach to (repeatable; default: every
    /// served profile).
    #[arg(long = "profile-binding", value_name = "PROFILE")]
    pub profiles: Vec<String>,
    /// `driver` (default), `observer` or `admin` (a driver that may also
    /// update the daemon with `daemon.update`).
    #[arg(long, default_value = "driver")]
    pub role: String,
}

fn role(text: &str) -> Result<DeviceRole, Error> {
    DeviceRole::parse(text).ok_or_else(|| {
        Error::new(
            &codes::CLI_USAGE,
            format!("--role '{text}' is not driver, observer or admin"),
        )
    })
}

/// Runs `daemon pair-device`.
///
/// # Errors
///
/// `cli.usage` for a bad name or role, `home.io`.
pub(crate) fn pair_device(home: &Path, args: &PairDeviceArgs, as_json: bool) -> Result<u8, Error> {
    let token = Devices::open(home).issue(&args.name, role(&args.role)?, &args.profiles)?;
    if as_json {
        return document(
            "daemon pair-device",
            &json!({"device": args.name, "role": args.role, "profiles": args.profiles, "token": token}),
        );
    }
    print_raw(&format!(
        "Paired device '{}'. Its token (shown once; keep it secret):\n{token}\n",
        args.name
    ))
}

/// Revokes `name` through the running daemon (closing its sockets), or in
/// the registry when no daemon runs.
///
/// # Errors
///
/// `device.not_found`, `home.io`, control channel errors.
pub(crate) fn revoke(home: &Path, name: &str, as_json: bool) -> Result<u8, Error> {
    match wait_running(home, Duration::from_secs(1))? {
        Some(record) => {
            let request = ControlRequest::RevokeDevice {
                name: name.to_owned(),
            };
            let response = control_call(&record, &request)?;
            if let Some(error) = &response.error {
                return Err(control_error(error));
            }
        }
        None => Devices::open(home).revoke(name)?,
    }
    if as_json {
        return document("devices revoke", &json!({"device": name, "revoked": true}));
    }
    print_raw(&format!("Revoked device '{name}'.\n"))
}
