//! `prometheus computer-use doctor` (spec: computer-use, Computer-use CLI
//! and doctor): the driver's structured health report (`health_report`)
//! folded into one matrix of checks — binary version, platform, session,
//! bundle identity, accessibility, screen recording, capture, telemetry
//! state, daemon — with the checks that do not apply to this platform
//! skipped. Exit 0 when every check passes, 1 when one is degraded or
//! failed, 69 when the driver cannot be reached.

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_config::daemon::DaemonConfig;
use pmt_tools::computer_use::Driver;
use serde::Serialize;
use serde_json::{Value, json};

use super::binary::{self, INSTALL_HINT, RUNTIME};
use super::platform::Facts;

/// The matrix's checks, in order.
pub const CHECKS: &[&str] = &[
    "binary_version",
    "platform",
    "session",
    "bundle_identity",
    "accessibility",
    "screen_recording",
    "capture",
    "telemetry",
    "daemon",
];

/// Checks only macOS has.
const MACOS_ONLY: &[&str] = &["bundle_identity", "accessibility", "screen_recording"];

/// The driver's names of our checks.
fn driver_names(check: &str) -> &'static [&'static str] {
    match check {
        "binary_version" => &["binary_version"],
        "platform" => &["platform_supported", "platform"],
        "session" => &["session_active", "session"],
        "bundle_identity" => &["bundle_identity"],
        "accessibility" => &["tcc_accessibility", "accessibility"],
        "screen_recording" => &["tcc_screen_recording", "screen_recording"],
        "capture" => &["capture", "screen_capture"],
        "telemetry" => &["telemetry"],
        _ => &["daemon"],
    }
}

/// One row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Row {
    /// The check.
    pub check: String,
    /// `pass`, `degraded`, `fail` or `skip`.
    pub status: String,
    /// What was found.
    pub message: String,
    /// What to do about it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

fn row(check: &str, status: &str, message: impl Into<String>) -> Row {
    Row {
        check: check.to_owned(),
        status: status.to_owned(),
        message: message.into(),
        hint: None,
    }
}

/// The doctor's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The matrix (exit 0 or 1).
    Report(Vec<Row>),
    /// The driver cannot be reached (exit 69), and why.
    Unreachable(String),
}

impl Outcome {
    /// The exit code.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Unreachable(_) => 69,
            Self::Report(rows)
                if rows
                    .iter()
                    .any(|r| matches!(r.status.as_str(), "fail" | "degraded")) =>
            {
                1
            }
            Self::Report(_) => 0,
        }
    }
}

/// Runs the doctor for `scope`; `include` / `skip` narrow the checks.
pub async fn run(
    scope: &ProfileScope,
    env: &[(String, OsString)],
    include: &[String],
    skip: &[String],
) -> Outcome {
    let config = &scope.config().computer_use;
    let daemon = match DaemonConfig::load(scope.home()) {
        Ok(daemon) => daemon,
        Err(err) => return Outcome::Unreachable(err.message().to_owned()),
    };
    let driver = match binary::installed(scope.home(), config, &daemon) {
        Ok(Some(driver)) => driver,
        Ok(None) => {
            return Outcome::Unreachable(format!("{RUNTIME} is not installed: {INSTALL_HINT}"));
        }
        Err(why) => return Outcome::Unreachable(format!("{why}: {INSTALL_HINT}")),
    };
    let manifest = binary::checked(&driver.path, env);
    let program = manifest
        .as_ref()
        .ok()
        .and_then(|m| m.mcp_command.clone())
        .unwrap_or_else(|| driver.path.clone());
    let args = manifest
        .as_ref()
        .map_or_else(|_| vec!["mcp".to_owned()], |m| m.mcp_args.clone());
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)));
    let mcp = match pmt_mcp::computer_use::McpDriver::open(
        command,
        std::time::Duration::from_secs(30),
        Box::new(()),
    )
    .await
    {
        Ok(mcp) => Arc::new(mcp) as Arc<dyn Driver>,
        Err(why) => return Outcome::Unreachable(format!("{RUNTIME} did not answer: {why}")),
    };
    let report = health_report(&mcp, include, skip).await;
    let facts =
        Facts::of(&|name: &str| env.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()));
    let ours = Ours {
        scope,
        env,
        driver: &driver.path,
        manifest: &manifest,
        facts: &facts,
        mcp: &mcp,
    };
    let mut rows = Vec::new();
    for check in CHECKS.iter().copied().filter(|c| wanted(c, include, skip)) {
        let reported = driver_names(check)
            .iter()
            .find_map(|name| report.get(*name).cloned());
        let mut found = match reported {
            _ if facts.os != "macos" && MACOS_ONLY.contains(&check) => {
                row(check, "skip", format!("not applicable on {}", facts.os))
            }
            Some(found) => Row {
                check: check.to_owned(),
                ..found
            },
            None => ours.check(check).await,
        };
        if found.hint.is_none() && matches!(found.status.as_str(), "fail" | "degraded") {
            found.hint = Some(format!(
                "see `prometheus computer-use {}`",
                if MACOS_ONLY.contains(&check) {
                    "permissions grant"
                } else {
                    "install --upgrade"
                }
            ));
        }
        rows.push(found);
    }
    Outcome::Report(rows)
}

fn wanted(check: &str, include: &[String], skip: &[String]) -> bool {
    (include.is_empty() || include.iter().any(|i| i == check)) && !skip.iter().any(|s| s == check)
}

/// The driver's `health_report` checks by name (empty when the driver has
/// none).
async fn health_report(
    mcp: &Arc<dyn Driver>,
    include: &[String],
    skip: &[String],
) -> std::collections::BTreeMap<String, Row> {
    let mut arguments = serde_json::Map::new();
    if !include.is_empty() {
        arguments.insert("include".into(), json!(include));
    }
    if !skip.is_empty() {
        arguments.insert("skip".into(), json!(skip));
    }
    let Ok(reply) = mcp.call("health_report", arguments).await else {
        return std::collections::BTreeMap::new();
    };
    reply
        .field("checks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|check| {
            let name = check["name"].as_str()?.to_owned();
            let status = match check["status"].as_str()? {
                "pass" | "ok" => "pass",
                "skip" => "skip",
                "degraded" | "warn" => "degraded",
                _ => "fail",
            };
            Some((
                name.clone(),
                Row {
                    check: name,
                    status: status.to_owned(),
                    message: check["message"].as_str().unwrap_or_default().to_owned(),
                    hint: check["hint"].as_str().map(str::to_owned),
                },
            ))
        })
        .collect()
}

/// The checks Prometheus makes itself when the driver reports none.
struct Ours<'a> {
    scope: &'a ProfileScope,
    env: &'a [(String, OsString)],
    driver: &'a Path,
    manifest: &'a Result<binary::Manifest, String>,
    facts: &'a Facts,
    mcp: &'a Arc<dyn Driver>,
}

impl Ours<'_> {
    async fn check(&self, check: &str) -> Row {
        match check {
            "binary_version" => match self.manifest {
                Ok(manifest) => row(
                    check,
                    "pass",
                    format!(
                        "{RUNTIME} {} at {}",
                        manifest.version,
                        self.driver.display()
                    ),
                ),
                Err(why) => row(check, "fail", why.clone()),
            },
            "platform" => {
                let supported = matches!(self.facts.os.as_str(), "macos" | "linux" | "windows");
                row(
                    check,
                    if supported { "pass" } else { "fail" },
                    format!("platform={}", self.facts.os),
                )
            }
            "session" => row(check, "skip", "the doctor opens no computer-use session"),
            "bundle_identity" => self.identity(),
            "accessibility" | "screen_recording" => self.grant(check),
            "capture" => match self.mcp.call("list_windows", serde_json::Map::new()).await {
                Ok(reply) if !reply.is_error => row(check, "pass", "window discovery answered"),
                Ok(reply) => row(check, "fail", reply.message()),
                Err(why) => row(check, "fail", why),
            },
            "telemetry" => {
                let on = self.scope.config().computer_use.driver_telemetry;
                row(
                    check,
                    "pass",
                    if on {
                        "driver telemetry enabled (computer_use.driver_telemetry)"
                    } else {
                        "driver telemetry disabled"
                    },
                )
            }
            _ => match binary::verb(self.driver, &["status"], self.env) {
                Ok(_) => row(check, "pass", "the machine-wide driver daemon is listening"),
                Err(_) => row(
                    check,
                    "skip",
                    "no machine-wide driver daemon (each session starts its own runtime)",
                ),
            },
        }
    }

    fn identity(&self) -> Row {
        let Some(app) = super::identity::bundle(self.driver) else {
            return row(
                "bundle_identity",
                "fail",
                format!("{} is not inside CuaDriver.app", self.driver.display()),
            );
        };
        match super::identity::verify(&app, self.scope.config().computer_use.allow_unsigned_driver)
        {
            Ok(()) => row(
                "bundle_identity",
                "pass",
                format!("{} is signed by the official identity", app.display()),
            ),
            Err(why) => row("bundle_identity", "fail", why),
        }
    }

    fn grant(&self, check: &str) -> Row {
        match permissions_report(self.driver, self.env) {
            Ok(report) if report[check].as_bool() == Some(true) => {
                row(check, "pass", format!("{check} granted"))
            }
            Ok(_) => row(check, "fail", format!("{check} missing")),
            Err(why) => row(check, "fail", why),
        }
    }
}

/// The driver's `permissions status --json`.
///
/// # Errors
///
/// The verb's failure or unreadable output.
pub fn permissions_report(driver: &Path, env: &[(String, OsString)]) -> Result<Value, String> {
    let text = binary::verb(driver, &["permissions", "status", "--json"], env)?;
    serde_json::from_str(text.trim()).map_err(|e| format!("unreadable permissions report: {e}"))
}

/// The matrix as text.
#[must_use]
pub fn render(rows: &[Row]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for row in rows {
        let _ = writeln!(out, "{:<17} {:<9} {}", row.check, row.status, row.message);
        if let Some(hint) = &row.hint {
            let _ = writeln!(out, "{:<17} {:<9} hint: {hint}", "", "");
        }
    }
    out
}
