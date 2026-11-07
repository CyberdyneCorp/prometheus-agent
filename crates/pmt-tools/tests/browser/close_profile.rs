//! `browser_close_profile` through the real dispatcher and approval gate,
//! against a stand-in "user's browser" process (spec:
//! execution-environments, Real-profile browsing).

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pmt_config::grants::{GrantsFile, SessionGrants};
use pmt_config::schema::{ApprovalsConfig, Config, SecurityConfig, UnattendedApproval};
use pmt_config::{Home, ProfileName};
use pmt_core::capability::{BASELINE, CapabilitySet};
use pmt_core::human::Channel;
use pmt_tools::approval::{ApprovalRequest, CommandGate, Context, Decision, Floors, GateSetup};
use pmt_tools::browser::close_profile::NAME;
use pmt_tools::browser::real_profile::{Browser, Detect};
use pmt_tools::environments::{Environments, SessionEnv};
use pmt_tools::protected::ProtectedFiles;
use pmt_tools::result::Status;
use serde_json::json;

use crate::dispatcher::{call, dispatcher, dispatcher_gated};
use crate::support::Fixture;

const SESSION: &str = "ses_b";

struct Fixed(Browser);

impl Detect for Fixed {
    fn default_browser(&self) -> Result<Browser, String> {
        Ok(self.0.clone())
    }
}

/// The user's browser: a `cat` waiting on a pipe and run "on" the user
/// data directory (`--user-data-dir=<dir>` among its arguments, read as a
/// file name after `--`), so ending it touches nothing else on the host.
/// Its executable is a path no process runs from.
struct StandIn {
    browser: Browser,
    child: Child,
}

impl StandIn {
    fn start(dir: &Path) -> Self {
        let user_data_dir = dir.join("user-data");
        let child = Command::new("cat")
            .args(["--", "-"])
            .arg(format!("--user-data-dir={}", user_data_dir.display()))
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        Self {
            browser: Browser {
                name: "standin".to_owned(),
                executable: dir.join("Stand-in Browser"),
                user_data_dir,
            },
            child,
        }
    }

    fn running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    fn ended_within(&mut self, limit: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < limit {
            if !self.running() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

impl Drop for StandIn {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A profile whose config offers the tool (or not, `autoclose = false`).
fn config(autoclose: bool) -> Config {
    let mut config = Config::default();
    config.browser.use_real_profile = true;
    config.browser.real_profile_autoclose = autoclose;
    config
}

fn gate(
    fixture: &Fixture,
    context: Context,
    channel: Channel<ApprovalRequest, Decision>,
) -> Arc<CommandGate> {
    let home = Home::open(&fixture.home.join("prometheus-home")).unwrap();
    let profile = home
        .profile(&ProfileName::parse("default").unwrap())
        .unwrap();
    let caps = CapabilitySet::parse(BASELINE.iter().copied()).unwrap();
    Arc::new(CommandGate::new(GateSetup {
        approvals: ApprovalsConfig {
            cron_mode: UnattendedApproval::Approve,
            ..ApprovalsConfig::default()
        },
        context,
        floors: Floors::new(&fixture.home),
        grants: Arc::new(SessionGrants::new(caps, GrantsFile::of(&profile))),
        protected: ProtectedFiles::from_config(&SecurityConfig::default()).unwrap(),
        channel,
        yolo: true,
        hooks: None,
    }))
}

/// Scenario: Closing the locked browser asks every time. With
/// `real_profile_autoclose` the call asks the user even under YOLO, never
/// offering to remember; a denial leaves the browser running, an approval
/// ends it, on the host; an unattended run is refused without asking.
#[tokio::test]
async fn closing_the_users_browser_asks_every_time_and_ends_it_once_approved() {
    let mut fixture = Fixture::new();
    let mut standin = StandIn::start(&fixture.home.join("standin"));
    fixture.detect = Arc::new(Fixed(standin.browser.clone()));
    {
        let mut browser = fixture.config.lock().unwrap();
        browser.use_real_profile = true;
        browser.real_profile_autoclose = true;
    }
    let manager = fixture.manager();
    let environments = Arc::new(Environments::default());
    let _attached = environments.attach(
        SESSION,
        SessionEnv {
            browser: Some(Arc::new(fixture.exec(&manager, false))),
            ..SessionEnv::default()
        },
    );
    let config = config(true);

    let cron = gate(&fixture, Context::Cron, Channel::unattended());
    let unattended =
        dispatcher_gated(&environments, SESSION, &config, &[], &fixture.profile, cron).await;
    let refused = call(&unattended, NAME, json!({})).await;
    assert_eq!(refused.status, Status::Denied, "{}", refused.text_content());
    assert!(standin.running(), "an unattended run never ends it");

    let (channel, mut prompts) = Channel::attached(Duration::from_mins(5));
    let asked = gate(&fixture, Context::Interactive, channel);
    let attended = dispatcher_gated(
        &environments,
        SESSION,
        &config,
        &[],
        &fixture.profile,
        asked,
    )
    .await;
    for (answer, ends) in [(Decision::Deny, false), (Decision::Once, true)] {
        let pending = {
            let attended = attended.clone();
            tokio::spawn(async move { call(&attended, NAME, json!({})).await })
        };
        let prompt = prompts.next().await.expect("asked under YOLO");
        assert_eq!(prompt.request().remember, None, "never remembered");
        prompt.respond(answer);
        let result = pending.await.unwrap();
        if ends {
            assert_eq!(result.status, Status::Ok, "{}", result.text_content());
            assert!(result.text_content().contains("ended 1 standin"));
            assert!(standin.ended_within(Duration::from_secs(10)));
        } else {
            assert_eq!(result.status, Status::Denied);
            assert!(standin.running(), "a denial leaves it running");
        }
    }
    manager.shutdown().await;
}

/// Scenario: Closing the locked browser asks every time (setting off):
/// without `real_profile_autoclose` the tool is not offered, and the
/// action refuses even when called directly.
#[tokio::test]
async fn without_autoclose_the_users_browser_is_never_ended() {
    let mut fixture = Fixture::new();
    let mut standin = StandIn::start(&fixture.home.join("standin"));
    fixture.detect = Arc::new(Fixed(standin.browser.clone()));
    fixture.config.lock().unwrap().use_real_profile = true;
    let manager = fixture.manager();
    let environments = Arc::new(Environments::default());
    let _attached = environments.attach(
        SESSION,
        SessionEnv {
            browser: Some(Arc::new(fixture.exec(&manager, false))),
            ..SessionEnv::default()
        },
    );
    let session = dispatcher(
        &environments,
        SESSION,
        &config(false),
        &[],
        &fixture.profile,
    )
    .await;
    let result = call(&session, NAME, json!({})).await;
    assert_ne!(result.status, Status::Ok, "{}", result.text_content());
    let refused = manager.close_real_profile(SESSION).await.unwrap_err();
    assert!(
        refused.contains("real_profile_autoclose is off"),
        "{refused}"
    );
    assert!(standin.running());
    manager.shutdown().await;
}
