//! `computer_use` against a scripted driver, through the real dispatcher
//! and approval gate (spec: computer-use).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_config::grants::{GrantsFile, SessionGrants};
use pmt_config::schema::{
    ApprovalsConfig, ComputerUseConfig, ComputerUsePermission, Config,
    ResultBudget as BudgetConfig, SecurityConfig, UnattendedApproval,
};
use pmt_config::{Home, ProfileDir, ProfileName};
use pmt_core::cancel::CancelToken;
use pmt_core::capability::{BASELINE, CapabilitySet};
use pmt_core::human::Channel;
use pmt_core::panic::CrashReporter;
use pmt_core::secret::Redactor;
use serde_json::{Map, Value, json};

use super::driver::{Driver, DriverReply, Launcher, PermissionMode, RuntimeSpec};
use super::session::{ComputerUseSession, SessionSetup};
use crate::approval::{CommandGate, Context, Decision, Floors, GateSetup};
use crate::budget::ResultBudget;
use crate::dispatch::{DispatchSetup, Dispatcher, NoHooks, NoObserver, SeamFuture, ToolCall};
use crate::environments::{Attachment, Environments, SessionEnv};
use crate::probe::ProbeCache;
use crate::protected::ProtectedFiles;
use crate::registry::{Builtins, ToolRegistry};
use crate::result::{Content, Status, ToolResult};
use crate::selection::Selection;
use crate::spillover::session_dir;
use crate::surface::{SurfaceInputs, ToolSurface};
use crate::variants::RouteTraits;

const SESSION: &str = "ses_01JCOMPUTERUSE";

/// What the fake driver saw.
#[derive(Default)]
struct Log {
    calls: Mutex<Vec<(String, Map<String, Value>)>>,
    launched: Mutex<Vec<PermissionMode>>,
    stopped: Mutex<Vec<PermissionMode>>,
}

impl Log {
    fn calls(&self, tool: &str) -> Vec<Map<String, Value>> {
        let calls = self.calls.lock().unwrap();
        calls
            .iter()
            .filter(|(t, _)| t == tool)
            .map(|(_, a)| a.clone())
            .collect()
    }

    fn input_calls(&self) -> usize {
        let calls = self.calls.lock().unwrap();
        calls
            .iter()
            .filter(|(t, _)| {
                [
                    "click",
                    "type_text",
                    "hotkey",
                    "press_key",
                    "drag",
                    "scroll",
                ]
                .contains(&t.as_str())
            })
            .count()
    }
}

/// A desktop with Mail (frontmost, 40 elements) and an editor.
struct FakeDriver {
    log: Arc<Log>,
    mode: PermissionMode,
    elements: usize,
    image: Vec<u8>,
}

impl Drop for FakeDriver {
    fn drop(&mut self) {
        self.log.stopped.lock().unwrap().push(self.mode);
    }
}

impl Driver for FakeDriver {
    fn call<'a>(
        &'a self,
        tool: &'a str,
        arguments: Map<String, Value>,
    ) -> SeamFuture<'a, Result<DriverReply, String>> {
        self.log
            .calls
            .lock()
            .unwrap()
            .push((tool.to_owned(), arguments));
        let reply = match tool {
            "list_windows" => DriverReply {
                structured: json!({"windows": [
                    {"pid": 11, "window_id": 101, "app_name": "Mail", "title": "Inbox", "z_index": 2},
                    {"pid": 22, "window_id": 202, "app_name": "Editor", "title": "notes.txt", "z_index": 1}
                ]}),
                ..DriverReply::default()
            },
            "get_window_state" => {
                let elements: Vec<Value> = (1..=self.elements)
                    .map(|i| json!({"element_index": i, "role": "AXButton", "label": format!("Button {i}"), "frame": {"x": 1, "y": 2, "w": 3, "h": 4}, "element_token": format!("s1:{i}")}))
                    .collect();
                DriverReply {
                    structured: json!({"elements": elements, "window_title": "Inbox"}),
                    images: vec![("image/png".to_owned(), self.image.clone())],
                    ..DriverReply::default()
                }
            }
            _ => DriverReply {
                structured: json!({"message": format!("{tool} done")}),
                ..DriverReply::default()
            },
        };
        Box::pin(async move { Ok(reply) })
    }

    fn tools(&self) -> Vec<String> {
        vec![
            "click".into(),
            "get_window_state".into(),
            "list_windows".into(),
        ]
    }
}

struct FakeLauncher {
    log: Arc<Log>,
    elements: usize,
    image: Vec<u8>,
}

impl Launcher for FakeLauncher {
    fn launch<'a>(
        &'a self,
        spec: &'a RuntimeSpec,
    ) -> SeamFuture<'a, Result<Arc<dyn Driver>, String>> {
        self.log.launched.lock().unwrap().push(spec.mode);
        let driver: Arc<dyn Driver> = Arc::new(FakeDriver {
            log: Arc::clone(&self.log),
            mode: spec.mode,
            elements: self.elements,
            image: self.image.clone(),
        });
        Box::pin(async move { Ok(driver) })
    }
}

/// A `width` x `height` PNG.
fn png(width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbImage::new(width, height);
    let mut out = std::io::Cursor::new(Vec::new());
    image.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

struct Harness {
    _temp: tempfile::TempDir,
    profile: ProfileDir,
    log: Arc<Log>,
    session: Arc<ComputerUseSession>,
    yolo: Arc<AtomicBool>,
    dispatcher: Dispatcher,
    _attached: Attachment,
}

struct Options {
    config: ComputerUseConfig,
    approvals: ApprovalsConfig,
    context: Context,
    channel: Channel<crate::approval::ApprovalRequest, crate::approval::Reply>,
    elements: usize,
    image: Vec<u8>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            config: ComputerUseConfig::default(),
            approvals: ApprovalsConfig::default(),
            context: Context::Interactive,
            channel: Channel::unattended(),
            elements: 40,
            image: png(64, 48),
        }
    }
}

async fn harness(options: Options) -> Harness {
    let temp = tempfile::tempdir().unwrap();
    let home = Home::open(&temp.path().join("home")).unwrap();
    let profile = home
        .profile(&ProfileName::parse("default").unwrap())
        .unwrap();
    let log = Arc::new(Log::default());
    let session = Arc::new(ComputerUseSession::new(SessionSetup {
        session_id: SESSION.to_owned(),
        profile_dir: profile.path().to_path_buf(),
        config: options.config,
        image_input: true,
        overlay: false,
        launcher: Arc::new(FakeLauncher {
            log: Arc::clone(&log),
            elements: options.elements,
            image: options.image,
        }),
    }));
    let yolo = Arc::new(AtomicBool::new(false));
    let probe = Arc::clone(&yolo);
    session.bind_yolo(Arc::new(move || probe.load(Ordering::SeqCst)));
    let environments = Arc::new(Environments::default());
    let attached = environments.attach(
        SESSION,
        SessionEnv {
            computer_use: Some(Arc::clone(&session)),
            ..SessionEnv::default()
        },
    );
    let caps = CapabilitySet::parse(BASELINE.iter().copied()).unwrap();
    let gate = Arc::new(CommandGate::new(GateSetup {
        approvals: options.approvals,
        context: options.context,
        floors: Floors::new(profile.path()),
        grants: Arc::new(SessionGrants::new(caps.clone(), GrantsFile::of(&profile))),
        protected: ProtectedFiles::from_config(&SecurityConfig::default()).unwrap(),
        channel: options.channel,
        yolo: false,
        hooks: None,
    }));
    let registry = ToolRegistry::new(
        "default",
        Arc::new(Builtins::standard(vec![super::tool(&environments)]).unwrap()),
    );
    let selection = Selection::resolve(&registry, &["computer_use".to_owned()], &[]).unwrap();
    let surface = ToolSurface::build(SurfaceInputs {
        registry: &registry,
        selection: &selection,
        capabilities: &caps,
        config: &Config::default(),
        probes: &ProbeCache::new(),
        route: &RouteTraits::default(),
    })
    .await;
    let dispatcher = Dispatcher::new(DispatchSetup {
        session_id: SESSION.into(),
        surface: Arc::new(surface),
        capabilities: Arc::new(caps),
        hooks: Arc::new(NoHooks),
        gate: gate as Arc<dyn crate::dispatch::ApprovalGate>,
        observer: Arc::new(NoObserver),
        budget: ResultBudget::new(
            BudgetConfig::default(),
            None,
            session_dir(profile.path(), SESSION),
            Redactor::default(),
        ),
        reporter: Arc::new(CrashReporter::new(
            profile.path().join("crash"),
            Redactor::default(),
        )),
        default_timeout: Duration::from_mins(1),
        concurrency: 1,
    });
    Harness {
        _temp: temp,
        profile,
        log,
        session,
        yolo,
        dispatcher,
        _attached: attached,
    }
}

impl Harness {
    async fn call(&self, arguments: Value) -> ToolResult {
        self.dispatcher
            .dispatch(
                ToolCall::new("call_1", super::NAME, &arguments.to_string()),
                &CancelToken::new(),
            )
            .await
    }
}

/// Approves every prompt with `decision`.
fn approving(
    decision: Decision,
) -> Channel<crate::approval::ApprovalRequest, crate::approval::Reply> {
    let (channel, mut prompts) = Channel::attached(Duration::from_mins(1));
    pmt_core::spawn::spawn(async move {
        while let Some(prompt) = prompts.next().await {
            prompt.respond(decision.into());
        }
    });
    channel
}

/// Scenario: Background click.
#[tokio::test]
async fn a_click_lands_in_the_target_window_in_the_background() {
    let h = harness(Options {
        channel: approving(Decision::Once),
        ..Options::default()
    })
    .await;
    let captured = h.call(json!({"action": "capture", "app": "Mail"})).await;
    assert_eq!(captured.status, Status::Ok, "{}", captured.text_content());
    let clicked = h.call(json!({"action": "click", "element": 14})).await;
    assert_eq!(clicked.status, Status::Ok, "{}", clicked.text_content());
    let clicks = h.log.calls("click");
    assert_eq!(clicks.len(), 1);
    let click = &clicks[0];
    assert_eq!(click["pid"], 11, "addressed to Mail's process");
    assert_eq!(click["window_id"], 101);
    assert_eq!(click["element_index"], 14);
    assert_eq!(click["element_token"], "s1:14");
    assert!(
        !click.contains_key("delivery_mode"),
        "background delivery is the default"
    );
    assert!(
        h.log.calls("bring_to_front").is_empty(),
        "the window is not raised"
    );
}

/// Scenario: Stale element.
#[tokio::test]
async fn an_element_missing_from_the_latest_capture_is_stale() {
    let h = harness(Options {
        channel: approving(Decision::Once),
        ..Options::default()
    })
    .await;
    h.call(json!({"action": "capture"})).await;
    let result = h.call(json!({"action": "click", "element": 90})).await;
    assert!(
        result
            .text_content()
            .contains("stale element 90; capture again"),
        "{}",
        result.text_content()
    );
    assert_eq!(h.log.input_calls(), 0);
}

#[tokio::test]
async fn an_unknown_action_is_named() {
    let h = harness(Options::default()).await;
    let result = h.call(json!({"action": "hover"})).await;
    assert!(
        result
            .text_content()
            .contains("unknown computer_use action 'hover'"),
        "{}",
        result.text_content()
    );
}

/// Scenario: Text-only model.
#[test]
fn a_text_only_model_is_offered_only_ax_captures() {
    let tool = super::tool(&Arc::new(Environments::default()));
    let text_only = RouteTraits::default().with_image_input(false);
    let advertised = tool.advertised(&Config::default(), &text_only);
    assert_eq!(
        advertised.parameters["properties"]["mode"]["enum"],
        json!(["ax"])
    );
    let vision = tool.advertised(
        &Config::default(),
        &RouteTraits::default().with_image_input(true),
    );
    assert_eq!(
        vision.parameters["properties"]["mode"]["enum"],
        json!(["som", "vision", "ax"])
    );
}

/// Scenario: Cron run.
#[tokio::test]
async fn cron_runs_cannot_type_unless_approved_by_policy() {
    let h = harness(Options {
        context: Context::Cron,
        approvals: ApprovalsConfig {
            cron_mode: UnattendedApproval::Deny,
            ..ApprovalsConfig::default()
        },
        ..Options::default()
    })
    .await;
    h.call(json!({"action": "capture"})).await;
    let result = h.call(json!({"action": "type", "text": "hello"})).await;
    assert_ne!(result.status, Status::Ok);
    assert!(
        result.text_content().contains("approvals.cron_mode"),
        "{}",
        result.text_content()
    );
    assert!(h.log.calls("type_text").is_empty(), "nothing typed");
    // Captures need no approval, even in cron.
    assert_eq!(
        h.call(json!({"action": "capture"})).await.status,
        Status::Ok
    );
}

#[tokio::test]
async fn a_session_grant_covers_one_action_in_one_delivery_mode() {
    let asked: Arc<Mutex<Vec<String>>> = Arc::default();
    let (channel, mut prompts) =
        Channel::<crate::approval::ApprovalRequest, crate::approval::Reply>::attached(
            Duration::from_mins(1),
        );
    let seen = Arc::clone(&asked);
    pmt_core::spawn::spawn(async move {
        while let Some(prompt) = prompts.next().await {
            let key = prompt.request().remember.clone().unwrap_or_default();
            seen.lock().unwrap().push(key);
            prompt.respond(Decision::Session.into());
        }
    });
    let h = harness(Options {
        channel,
        ..Options::default()
    })
    .await;
    h.call(json!({"action": "capture"})).await;
    for arguments in [
        json!({"action": "click", "x": 1, "y": 2}),
        json!({"action": "click", "x": 5, "y": 6}),
        json!({"action": "click", "x": 3, "y": 4, "delivery_mode": "foreground"}),
    ] {
        assert_eq!(h.call(arguments).await.status, Status::Ok);
    }
    assert_eq!(
        *asked.lock().unwrap(),
        vec![
            "computer_use:click:background",
            "computer_use:click:foreground"
        ],
        "the background grant answered the second click, not the foreground one"
    );
    assert_eq!(h.log.calls("click")[2]["delivery_mode"], "foreground");
}

/// Scenario: Lock screen under yolo.
#[tokio::test]
async fn the_lock_screen_shortcut_is_refused_under_yolo() {
    let h = harness(Options::default()).await;
    h.yolo.store(true, Ordering::SeqCst);
    h.call(json!({"action": "capture"})).await;
    let result = h.call(json!({"action": "key", "keys": "cmd+ctrl+q"})).await;
    assert_eq!(result.status, Status::Denied);
    assert!(
        result.text_content().contains("hard block"),
        "{}",
        result.text_content()
    );
    assert!(result.text_content().contains("locks the screen"));
    assert_eq!(h.log.input_calls(), 0);
}

/// Scenario: Yolo turned off.
#[tokio::test]
async fn turning_yolo_off_stops_the_unrestricted_runtime() {
    let h = harness(Options::default()).await;
    h.yolo.store(true, Ordering::SeqCst);
    h.call(json!({"action": "list_windows"})).await;
    assert_eq!(
        h.session.running_mode().await,
        Some(PermissionMode::Unrestricted)
    );
    h.yolo.store(false, Ordering::SeqCst);
    h.session.reconcile().await;
    assert_eq!(
        *h.log.stopped.lock().unwrap(),
        vec![PermissionMode::Unrestricted]
    );
    assert_eq!(h.session.running_mode().await, None);
    h.call(json!({"action": "list_windows"})).await;
    assert_eq!(
        *h.log.launched.lock().unwrap(),
        vec![PermissionMode::Unrestricted, PermissionMode::Standard]
    );
}

#[tokio::test]
async fn a_bounded_profile_launches_bounded_runtimes() {
    let h = harness(Options {
        config: ComputerUseConfig {
            permission_mode: ComputerUsePermission::Bounded,
            ..ComputerUseConfig::default()
        },
        ..Options::default()
    })
    .await;
    h.call(json!({"action": "list_apps"})).await;
    assert_eq!(
        *h.log.launched.lock().unwrap(),
        vec![PermissionMode::Bounded]
    );
    let setup = h.log.calls("set_agent_cursor_enabled");
    assert_eq!(setup[0]["enabled"], false);
    assert_eq!(
        setup[0]["cursor_id"],
        h.session.label(),
        "the overlay is labelled per session"
    );
}

#[tokio::test]
async fn captures_are_budgeted_and_kept() {
    let h = harness(Options {
        elements: 150,
        image: png(3000, 1500),
        ..Options::default()
    })
    .await;
    let result = h.call(json!({"action": "capture", "mode": "som"})).await;
    let body: Value = serde_json::from_str(&result.text_content()).unwrap();
    assert_eq!(body["elements"].as_array().unwrap().len(), 100);
    assert_eq!(body["total_elements"], 150);
    let spilled = std::fs::read_to_string(body["elements_file"].as_str().unwrap()).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&spilled)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        150
    );
    assert_eq!(
        (body["width"].as_u64(), body["height"].as_u64()),
        (Some(1456), Some(728))
    );
    let path = body["screenshot_path"].as_str().unwrap();
    assert!(
        path.starts_with(
            &h.profile
                .path()
                .join("cache")
                .join("computer-use")
                .display()
                .to_string()
        )
    );
    assert!(matches!(&result.content[1], Content::Media { path: p, .. } if p == path));
    let ax = h.call(json!({"action": "capture", "mode": "ax"})).await;
    assert_eq!(ax.content.len(), 1, "an ax capture carries no image");
}

#[tokio::test]
async fn only_the_newest_screenshots_are_kept() {
    let h = harness(Options::default()).await;
    for _ in 0..25 {
        h.call(json!({"action": "capture", "mode": "vision"})).await;
    }
    let dir = h.profile.path().join("cache").join("computer-use");
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        super::capture::KEEP_SCREENSHOTS
    );
    let latest = super::capture::latest_screenshot(h.profile.path()).unwrap();
    let newest = h.call(json!({"action": "capture", "mode": "vision"})).await;
    let body: Value = serde_json::from_str(&newest.text_content()).unwrap();
    assert_ne!(
        body["screenshot_path"].as_str().unwrap(),
        latest.display().to_string()
    );
    assert_eq!(
        super::capture::latest_screenshot(h.profile.path())
            .unwrap()
            .display()
            .to_string(),
        body["screenshot_path"].as_str().unwrap()
    );
}
