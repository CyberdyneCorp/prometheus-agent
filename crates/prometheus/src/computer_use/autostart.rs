//! `computer_use.autostart` (spec: computer-use, Platform prerequisites):
//! on Windows a daemon in the services' Session 0 cannot drive the
//! desktop, so with autostart on it registers a per-logon scheduled task
//! that runs the driver in the user's interactive session. The
//! prerequisite holds only once that task exists; until then the tool
//! reports how to fix it.

use std::path::Path;

use pmt_config::schema::ComputerUseConfig;

use super::platform::Facts;

/// The scheduled task's name.
pub const TASK: &str = "PrometheusCuaDriver";

/// The `schtasks` arguments registering the per-logon driver task.
#[must_use]
pub fn create_args(driver: &Path) -> Vec<String> {
    vec![
        "/Create".to_owned(),
        "/F".to_owned(),
        "/SC".to_owned(),
        "ONLOGON".to_owned(),
        "/RL".to_owned(),
        "LIMITED".to_owned(),
        "/TN".to_owned(),
        TASK.to_owned(),
        "/TR".to_owned(),
        format!("\"{}\" serve", driver.display()),
    ]
}

/// Whether the task must be registered: Windows, outside an interactive
/// session, autostart on, and no task found yet.
#[must_use]
pub fn needed(config: &ComputerUseConfig, facts: &Facts) -> bool {
    facts.os == "windows" && !facts.interactive && config.autostart && !facts.autostart_task
}

/// Registers the task for `driver` when [`needed`], then records in
/// `facts` whether it now exists.
///
/// # Errors
///
/// Why `schtasks` could not register it.
pub fn ensure(config: &ComputerUseConfig, facts: &mut Facts, driver: &Path) -> Result<(), String> {
    if !needed(config, facts) {
        return Ok(());
    }
    schtasks(&create_args(driver))?;
    facts.autostart_task = registered();
    if facts.autostart_task {
        Ok(())
    } else {
        Err(format!(
            "the scheduled task {TASK} was not found after it was created"
        ))
    }
}

/// Whether the task is registered (`schtasks /Query /TN`).
#[must_use]
pub fn registered() -> bool {
    cfg!(windows) && schtasks(&["/Query".to_owned(), "/TN".to_owned(), TASK.to_owned()]).is_ok()
}

#[cfg(windows)]
fn schtasks(args: &[String]) -> Result<(), String> {
    let mut command = std::process::Command::new("schtasks");
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let output = pmt_sandbox::container::run_bounded(command, std::time::Duration::from_secs(15))
        .map_err(|e| format!("schtasks {}: {e}", args.join(" ")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "schtasks {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[cfg(not(windows))]
fn schtasks(_args: &[String]) -> Result<(), String> {
    Err("scheduled tasks exist only on Windows".to_owned())
}

#[cfg(test)]
mod tests {
    use pmt_config::schema::ComputerUseConfig;

    use super::{Facts, ensure, needed};

    fn service() -> Facts {
        Facts {
            os: "windows".into(),
            ..Facts::default()
        }
    }

    #[test]
    fn only_a_service_session_with_autostart_needs_the_task() {
        let on = ComputerUseConfig {
            autostart: true,
            ..ComputerUseConfig::default()
        };
        assert!(needed(&on, &service()));
        assert!(!needed(&ComputerUseConfig::default(), &service()));
        let interactive = Facts {
            interactive: true,
            ..service()
        };
        assert!(!needed(&on, &interactive));
        let registered = Facts {
            autostart_task: true,
            ..service()
        };
        assert!(!needed(&on, &registered));
        let mac = Facts {
            os: "macos".into(),
            ..Facts::default()
        };
        assert!(!needed(&on, &mac));
        let mut nothing_to_do = mac;
        assert_eq!(
            ensure(&on, &mut nothing_to_do, std::path::Path::new("cua")),
            Ok(())
        );
    }

    /// On Windows the task really is registered and found, then removed.
    #[cfg(windows)]
    #[test]
    fn the_task_is_registered_and_queried() {
        let on = ComputerUseConfig {
            autostart: true,
            ..ComputerUseConfig::default()
        };
        let mut facts = service();
        let driver = std::env::current_exe().unwrap();
        ensure(&on, &mut facts, &driver).unwrap();
        assert!(facts.autostart_task);
        assert!(super::registered());
        super::schtasks(&[
            "/Delete".to_owned(),
            "/TN".to_owned(),
            super::TASK.to_owned(),
            "/F".to_owned(),
        ])
        .unwrap();
        assert!(!super::registered());
    }
}
