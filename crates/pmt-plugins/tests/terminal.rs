//! `terminal-backend` plugins as terminal backends (spec:
//! execution-environments, Remote sandboxes are plugins; Built-in backend
//! names are reserved; Backend isolation descriptor).

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::{Fake, Profile};
use pmt_plugins::host::ports::{HttpPort, HttpRequest, HttpResponse, PortFuture};
use pmt_plugins::host::{CallInfo, Runtime};
use pmt_plugins::terminal::registry;
use pmt_sandbox::backend::{ExecRequest, Isolation, RemoteBackend};

const WORLD: &str = "\"terminal-backend@1.1\"";

fn request(line: &str) -> ExecRequest {
    ExecRequest {
        id: "c1".to_owned(),
        command: line.to_owned(),
        cwd: "/sandbox".to_owned(),
        env: vec![("LANG".to_owned(), "C.UTF-8".to_owned())],
        timeout_ms: 5_000,
    }
}

#[test]
fn the_sample_backend_registers_and_answers_every_call() {
    let mut profile = Profile::new("p");
    profile.install("sample-backend", "sample-terminal-backend", WORLD, "");
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &Fake::new().ports());
    let (registry, rejected) = registry(&loaded, &CallInfo::default());
    assert!(rejected.is_empty(), "{rejected:?}");
    let backend = registry.select("sample").unwrap();
    let plugin = backend.plugin.clone().unwrap();
    assert_eq!(plugin.descriptor().isolation, Isolation::Remote);
    assert_eq!(plugin.descriptor().default_cwd, "/sandbox");
    assert_eq!(plugin.check().unwrap(), "in-memory sample");
    let output = plugin.execute(&request("cd /sandbox/src")).unwrap();
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.stdout, "cd /sandbox/src\n");
    assert_eq!(output.cwd_observed.as_deref(), Some("/sandbox/src"));
    plugin.upload("/sandbox/a.txt", b"hello").unwrap();
    assert_eq!(plugin.download("/sandbox/a.txt").unwrap(), b"hello");
    plugin.cancel("c1");
    plugin.teardown();
}

#[test]
fn plugin_omits_exit_code_reaches_the_host_as_missing() {
    let mut profile = Profile::new("p");
    profile.install("testing-backend", "testing-backend", WORLD, "");
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &Fake::new().ports());
    let (registry, _) = registry(&loaded, &CallInfo::default());
    let plugin = registry.select("testbox").unwrap().plugin.clone().unwrap();
    let output = plugin.execute(&request("echo omit-exit-code")).unwrap();
    assert_eq!(output.exit_code, None);
    assert_eq!(plugin.execute(&request("true")).unwrap().exit_code, Some(3));
}

#[test]
fn relative_default_cwd() {
    let mut profile = Profile::new("p");
    profile.install("testing-backend", "testing-backend", WORLD, "");
    let fake = Fake::new();
    fake.set("testing-backend", "default_cwd", "\"work\"");
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &fake.ports());
    let (registry, rejected) = registry(&loaded, &CallInfo::default());
    assert_eq!(
        rejected[0].1.message(),
        "terminal backend 'testbox' rejected: default_cwd must be absolute or '~'"
    );
    assert!(registry.select("testbox").is_err());
}

#[test]
fn a_plugin_backend_named_like_a_builtin_is_rejected() {
    let mut profile = Profile::new("p");
    profile.install("testing-backend", "testing-backend", WORLD, "");
    let fake = Fake::new();
    fake.set("testing-backend", "name", "\"Docker\"");
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &fake.ports());
    let (registry, rejected) = registry(&loaded, &CallInfo::default());
    assert_eq!(
        rejected[0].1.message(),
        "terminal backend name 'Docker' is reserved"
    );
    assert_eq!(
        registry.select("container").unwrap().kind,
        pmt_sandbox::backend::Kind::Container
    );
}

/// A remote sandbox API whose every reply takes 50 ms: the testing
/// backend's `wait-for-cancel` command polls it until it is cancelled.
#[derive(Default)]
struct SlowApi {
    polls: AtomicUsize,
}

impl SlowApi {
    fn polls(&self) -> usize {
        self.polls.load(Ordering::SeqCst)
    }
}

impl HttpPort for SlowApi {
    fn send(&self, _: HttpRequest) -> PortFuture<'_, Result<HttpResponse, String>> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: Vec::new(),
            })
        })
    }
}

/// The testing backend on [`SlowApi`] with a 300 ms plugin call timeout.
struct Slow {
    plugin: Arc<dyn RemoteBackend>,
    api: Arc<SlowApi>,
    fake: Arc<Fake>,
    profile: Profile,
}

impl Slow {
    fn new() -> Self {
        let mut profile = Profile::new("p");
        profile.config.wasm.call_timeout_ms = 300;
        profile.install(
            "testing-backend",
            "testing-backend",
            WORLD,
            "network = [\"pause.example\"]",
        );
        let fake = Fake::new();
        let api = Arc::new(SlowApi::default());
        let mut ports = fake.ports();
        ports.http = Arc::clone(&api) as Arc<dyn HttpPort>;
        let runtime = Runtime::new().unwrap();
        let loaded = profile.load(&runtime, &ports);
        let (registry, rejected) = registry(&loaded, &CallInfo::default());
        assert!(rejected.is_empty(), "{rejected:?}");
        let plugin = registry.select("testbox").unwrap().plugin.clone().unwrap();
        Self {
            plugin,
            api,
            fake,
            profile,
        }
    }

    /// The ids `cancel` reached inside the plugin.
    fn cancelled(&self) -> Vec<String> {
        self.fake
            .settings
            .lock()
            .unwrap()
            .keys()
            .filter_map(|(_, key)| key.strip_prefix("cancelled_").map(str::to_owned))
            .collect()
    }

    /// Waits until the plugin's command stopped polling its API.
    fn wait_until_quiet(&self) {
        let mut last = self.api.polls();
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(200));
            let now = self.api.polls();
            if now == last {
                return;
            }
            last = now;
        }
        panic!("the plugin's command kept running");
    }
}

#[test]
fn a_long_execute_outlives_the_call_timeout_and_cancel_reaches_it() {
    let slow = Slow::new();
    let mut long = request("wait-for-cancel");
    long.id = "long".to_owned();
    long.timeout_ms = u64::MAX;
    let running = {
        let plugin = Arc::clone(&slow.plugin);
        std::thread::spawn(move || plugin.execute(&long))
    };
    // Well past the 300 ms call timeout the command still runs: time spent
    // waiting on the remote API is not plugin CPU.
    std::thread::sleep(Duration::from_secs(1));
    assert!(!running.is_finished());
    assert!(slow.api.polls() >= 5, "{} polls", slow.api.polls());
    // Other calls are not queued behind it, and cancel reaches it while it
    // runs.
    let started = Instant::now();
    assert_eq!(
        slow.plugin.execute(&request("true")).unwrap().exit_code,
        Some(3)
    );
    slow.plugin.cancel("long");
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "calls waited for the running execute: {:?}",
        started.elapsed()
    );
    let output = running.join().unwrap().unwrap();
    assert_eq!(output.exit_code, Some(130), "{output:?}");
    assert!(output.stdout.starts_with("cancelled after"), "{output:?}");
    assert_eq!(slow.cancelled(), ["long"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn killing_a_background_command_cancels_it_inside_the_plugin() {
    use pmt_config::schema::TerminalConfig;
    use pmt_tools::terminal::process::ProcessArgs;
    use pmt_tools::terminal::target::PluginTarget;
    use pmt_tools::terminal::{Status, Terminal, TerminalArgs, TerminalSetup};

    let slow = Slow::new();
    let dir = slow.profile.path().to_path_buf();
    let terminal = Terminal::new(TerminalSetup {
        terminal: TerminalConfig::default(),
        max_chars: pmt_tools::setting::fixed(50_000),
        kill_grace: Duration::from_secs(2),
        target: Arc::new(PluginTarget::new(Arc::clone(&slow.plugin))),
        env: pmt_tools::terminal::env::ChildEnv::default(),
        shell: pmt_tools::terminal::shell::resolve_unix(None),
        scratch: dir.join("scratch"),
        spill_dir: dir.join("spill"),
        start_cwd: "/box".into(),
        user_home: None,
        redactor: pmt_core::secret::Redactor::default(),
        hooks: Vec::new(),
        guards: pmt_tools::terminal::guard::GuardChain::default(),
        notices: Vec::new(),
        notify: None,
        clock: None,
        auto_notify: false,
        receipts: None,
        before_command: None,
    })
    .unwrap();
    let started = terminal
        .run(TerminalArgs {
            background: true,
            ..TerminalArgs::new("wait-for-cancel")
        })
        .await;
    assert_eq!(started.status, Status::Started, "{started:?}");
    let id = started.process_id.expect("a tracked process");
    tokio::time::sleep(Duration::from_secs(1)).await;
    // A foreground command runs while the background one waits.
    let foreground = terminal.run(TerminalArgs::new("echo hi")).await;
    assert_eq!(foreground.exit_code, Some(3), "{foreground:?}");
    let killed = terminal
        .processes()
        .handle(&ProcessArgs {
            action: "kill".to_owned(),
            process_id: Some(id),
            data: None,
            timeout: Some(30.0),
        })
        .await;
    assert_eq!(killed["state"], "killed", "{killed}");
    let quiet = tokio::task::spawn_blocking(move || {
        slow.wait_until_quiet();
        slow
    });
    let slow = quiet.await.unwrap();
    assert_eq!(slow.cancelled().len(), 1, "the plugin saw the cancel");
}
