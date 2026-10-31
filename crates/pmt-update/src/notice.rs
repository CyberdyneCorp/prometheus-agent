//! The background update check (spec: distribution-and-update, Update
//! check without telemetry): at most once per `update.check_interval_secs`
//! (`0` disables it, so no request is ever made), fetching only the signed
//! channel index; a newer release becomes a one-line notice and is never
//! installed by the check itself.

use std::path::{Path, PathBuf};

use pmt_config::daemon::UpdateConfig;
use pmt_core::error::Error;
use pmt_core::private_fs;
use serde::{Deserialize, Serialize};

use crate::fetch::Releases;
use crate::flow::{Checked, Install, Session, check};
use crate::install::{create_dirs, io_error};

const STATE_FILE: &str = "last_check.json";

/// What the last background check found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CheckState {
    /// Unix seconds of the check.
    checked_at: i64,
    /// The notice it produced, if any.
    notice: Option<String>,
}

/// Runs the background check when due and returns the notice to show
/// (`update available: vA → vB; run: prometheus update`). Installs owned
/// elsewhere and failed checks show nothing.
///
/// # Errors
///
/// `update.failed` when the check state cannot be written.
///
/// # Panics
///
/// Never: the check state always serializes.
pub async fn background(
    install: &Install,
    config: &UpdateConfig,
    releases: &Releases,
    now: jiff::Timestamp,
) -> Result<Option<String>, Error> {
    if config.check_interval_secs == 0 {
        return Ok(None);
    }
    let path = state_path(install.home.root());
    let state: Option<CheckState> = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let interval = i64::try_from(config.check_interval_secs).unwrap_or(i64::MAX);
    if let Some(state) = state.filter(|s| now.as_second() - s.checked_at < interval) {
        return Ok(state.notice);
    }
    let quiet = |_: &str| {};
    let session = Session {
        install,
        config,
        releases,
        progress: &quiet,
    };
    let notice = match check(&session, None).await {
        Ok(checked @ Checked::Available { .. }) => {
            Some(format!("{}; run: prometheus update", checked.message()))
        }
        _ => None,
    };
    let state = CheckState {
        checked_at: now.as_second(),
        notice: notice.clone(),
    };
    let json = serde_json::to_vec(&state).expect("check state serializes");
    if let Some(dir) = path.parent() {
        create_dirs(dir)?;
    }
    private_fs::write_atomic(&path, &json).map_err(|e| io_error(&path, &e))?;
    Ok(notice)
}

fn state_path(home: &Path) -> PathBuf {
    home.join("updates").join(STATE_FILE)
}
