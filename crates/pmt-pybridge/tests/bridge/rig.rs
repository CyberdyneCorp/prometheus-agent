//! A profile home with one bridged plugin and its supervisor.

use std::path::PathBuf;

use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_config::schema::{PluginsConfig, PythonBridgeConfig};
use pmt_core::error::Error;
use pmt_pybridge::env;
use pmt_pybridge::launch::{self, LaunchEnv, LoadParams};
use pmt_pybridge::log::Log;
use pmt_pybridge::runtime;
use pmt_pybridge::shim;
use pmt_pybridge::sidecar::{HostFault, HostHandler};
use pmt_pybridge::supervisor::{Launcher, PluginSpec, Settings, Supervisor};
use pmt_pybridge::testkit;
use serde_json::Value;

/// Records `host.call` requests and answers `console.*`.
#[derive(Default)]
pub(crate) struct Recorder {
    pub(crate) calls: Mutex<Vec<(String, Value)>>,
}

impl HostHandler for Recorder {
    fn handle(&self, method: &str, args: Value, _context: &Value) -> Result<Value, HostFault> {
        self.calls.lock().unwrap().push((method.to_owned(), args));
        Ok(Value::Null)
    }
}

struct Plain {
    python: PathBuf,
    shim: PathBuf,
    env: LaunchEnv,
}

impl Launcher for Plain {
    fn command(&self) -> Result<Command, Error> {
        let mut command = Command::new(&self.python);
        launch::configure(&mut command, &self.shim, &self.env);
        Ok(command)
    }
}

pub(crate) struct Rig {
    _home: tempfile::TempDir,
    pub(crate) data: PathBuf,
    pub(crate) host: Arc<Recorder>,
    pub(crate) supervisor: Arc<Supervisor>,
    pub(crate) log: PathBuf,
}

/// Fast settings: 200 ms pings, short backoff.
pub(crate) fn quick() -> Settings {
    let mut settings =
        Settings::from_config(&PythonBridgeConfig::default(), &PluginsConfig::default());
    settings.ping_every = Duration::from_millis(200);
    settings.backoff_start = Duration::from_millis(50);
    settings.backoff_max = Duration::from_millis(200);
    settings.call_timeout = Duration::from_secs(20);
    settings
}

pub(crate) fn rig(plugin: &str, settings: Settings) -> Rig {
    rig_with(plugin, settings, Vec::new())
}

pub(crate) fn rig_with(plugin: &str, settings: Settings, secrets: Vec<(String, String)>) -> Rig {
    let home = tempfile::tempdir().unwrap();
    testkit::seed(home.path());
    let profile = home.path().join("profiles/default");
    let dir = testkit::place(home.path(), plugin, plugin);
    let uv = runtime::Uv::of(home.path()).unwrap();
    let python = runtime::require(home.path(), testkit::VERSION).unwrap();
    let lock = env::lock(
        &uv,
        &python,
        &[],
        &profile.join("python/locks").join(plugin),
    )
    .unwrap();
    let built = env::ensure(
        &uv,
        &python,
        &profile.join("python/envs"),
        plugin,
        &lock,
        true,
    )
    .unwrap();
    let shim = shim::materialize(&profile.join("python/shim")).unwrap();
    let data = profile.join("plugin-data").join(plugin);
    let tmp = profile.join("cache/bridge-tmp").join(plugin);
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&tmp).unwrap();
    let names: Vec<String> = secrets.iter().map(|(n, _)| n.clone()).collect();
    let launcher = Arc::new(Plain {
        python: built.python.clone(),
        shim,
        env: LaunchEnv {
            home: data.clone(),
            tmp,
            path: vec![built.bin.clone()],
            secrets,
            proxy: None,
        },
    });
    let load = LoadParams {
        plugin,
        profile: "default",
        dir: &dir,
        data_dir: &data,
        secrets: names,
        capabilities: Vec::new(),
        manifest: serde_json::json!({"name": plugin}),
    }
    .to_json();
    let log_path = Log::path_of(&profile, plugin);
    let host = Arc::new(Recorder::default());
    let supervisor = Supervisor::new(
        PluginSpec {
            id: plugin.to_owned(),
            profile: "default".to_owned(),
            profile_dir: profile.clone(),
            load,
            launcher,
            host: host.clone(),
            log: Arc::new(Log::new(log_path.clone())),
            cache: profile.join("cache/bridge").join(format!("{plugin}.json")),
        },
        settings,
    );
    Rig {
        _home: home,
        data,
        host,
        supervisor,
        log: log_path,
    }
}

/// Waits up to `bound` for `check`.
pub(crate) fn eventually(bound: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + bound;
    while std::time::Instant::now() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    check()
}

/// Whether process `pid` still exists (and is not a zombie).
pub(crate) fn alive(pid: u32) -> bool {
    let mut system = sysinfo::System::new();
    let pid = sysinfo::Pid::from_u32(pid);
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).is_some_and(|p| {
        !matches!(
            p.status(),
            sysinfo::ProcessStatus::Zombie | sysinfo::ProcessStatus::Dead
        )
    })
}
