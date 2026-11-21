//! Supervised sidecars of adapter plugins (spec: platform-adapters,
//! Supervised sidecar adapters).
//!
//! A plugin's `[sidecar]` runs when its adapter connects: on the managed
//! Node runtime (its packages installed from the plugin's lockfile with
//! `npm ci` under `<home>/runtimes/sidecars/<plugin>/`) or as an
//! executable the plugin ships. It serves a loopback port chosen per
//! start (`{port}` in its arguments, `PMT_SIDECAR_PORT`) and is reached
//! with a bearer token made per start (`PMT_SIDECAR_TOKEN`). Its
//! environment holds only those, `PATH` and the secrets its `[sidecar]`
//! lists that the plugin was granted: never other gateway or provider
//! secrets. The port and token are published in
//! `<home>/run/sidecars/<profile>-<plugin>.json` (owner-only) for
//! senders outside the daemon. An exit the adapter did not ask for puts it
//! in `retrying` with code `sidecar_exited`, so the gateway reconnects it
//! with its backoff (starting the sidecar again); disconnecting stops it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pmt_core::capability::{Capability, CapabilitySet};
use pmt_gateway::messaging::{AdapterError, AdapterLink, AdapterState};
use pmt_plugins::manifest::sidecar::{Runtime, SidecarDeclaration};
use serde_json::json;
use tokio::sync::oneshot;

/// The error code of an unexpected exit.
pub(crate) const EXITED: &str = "sidecar_exited";

/// What starts one plugin's sidecar.
#[derive(Debug, Clone)]
pub(crate) struct Spec {
    pub home: PathBuf,
    pub profile: String,
    pub plugin: String,
    pub plugin_dir: PathBuf,
    pub declaration: SidecarDeclaration,
    /// The secrets it receives (declared and granted), with their values.
    pub secrets: Vec<(String, String)>,
    /// The managed Node executable (`runtime = "node"`).
    pub node: Option<PathBuf>,
}

/// A running sidecar; dropping it stops the process.
pub(crate) struct Running {
    pub port: u16,
    pub token: String,
    run_file: PathBuf,
    /// Tells the watcher the stop is wanted (no `sidecar_exited`).
    stop: Option<oneshot::Sender<()>>,
}

impl Running {
    /// What the adapter is told (merged into its start options).
    pub(crate) fn options(&self) -> serde_json::Value {
        json!({"url": format!("http://127.0.0.1:{}", self.port), "token": self.token})
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let _ = std::fs::remove_file(&self.run_file);
    }
}

/// The secrets a sidecar may receive: those its declaration lists that
/// the plugin's live grant allows.
pub(crate) fn granted_secrets(
    declaration: &SidecarDeclaration,
    grants: &CapabilitySet,
) -> Vec<String> {
    declaration
        .secrets
        .iter()
        .filter(|name| grants.allows(&Capability::SecretsRead((*name).clone())))
        .cloned()
        .collect()
}

/// Where its port and token are published.
pub(crate) fn run_file(home: &Path, profile: &str, plugin: &str) -> PathBuf {
    home.join("run")
        .join("sidecars")
        .join(format!("{profile}-{plugin}.json"))
}

fn failed(message: String) -> AdapterError {
    AdapterError {
        retryable: true,
        code: "sidecar_start".to_owned(),
        message,
    }
}

fn free_port() -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

fn new_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the OS random generator is available");
    hex(&bytes)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// The program and arguments of `spec` on `port`.
fn command_line(spec: &Spec, port: u16) -> Result<(PathBuf, Vec<String>), AdapterError> {
    let script = spec.plugin_dir.join(&spec.declaration.command);
    let args = spec
        .declaration
        .args
        .iter()
        .map(|a| a.replace("{port}", &port.to_string()));
    match spec.declaration.runtime {
        Runtime::None => Ok((script, args.collect())),
        Runtime::Node => {
            let node = spec
                .node
                .clone()
                .ok_or_else(|| failed("the managed Node runtime is not installed".to_owned()))?;
            let mut all = vec![script.to_string_lossy().into_owned()];
            all.extend(args);
            Ok((node, all))
        }
    }
}

/// Starts `spec`'s sidecar and waits until its port accepts connections;
/// an exit nobody asked for is reported to `link`.
pub(crate) async fn start(
    spec: &Spec,
    link: Arc<dyn AdapterLink>,
) -> Result<Running, AdapterError> {
    let port = free_port().map_err(|e| failed(format!("no loopback port: {e}")))?;
    let token = new_token();
    let (program, args) = command_line(spec, port)?;
    let mut command = tokio::process::Command::new(&program);
    command
        .args(&args)
        .current_dir(&spec.plugin_dir)
        .env_clear()
        .envs(environment(spec, port, &token))
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|e| failed(format!("{}: {e}", program.display())))?;
    let ready = Duration::from_secs(spec.declaration.ready_timeout_secs);
    if let Err(why) = wait_ready(port, ready, &mut child).await {
        let _ = child.kill().await;
        return Err(failed(why));
    }
    let run_file = run_file(&spec.home, &spec.profile, &spec.plugin);
    publish(&run_file, port, &token, child.id())
        .map_err(|e| failed(format!("{}: {e}", run_file.display())))?;
    let (stop, stopped) = oneshot::channel();
    pmt_core::spawn::spawn(watch(child, stopped, link, spec.plugin.clone()));
    Ok(Running {
        port,
        token,
        run_file,
        stop: Some(stop),
    })
}

/// Its whole environment: the port, the token, `PATH` (and `NODE_PATH`
/// for Node) and its granted secrets; nothing inherited from the daemon.
fn environment(spec: &Spec, port: u16, token: &str) -> Vec<(String, std::ffi::OsString)> {
    let mut env: Vec<(String, std::ffi::OsString)> = vec![
        ("PMT_SIDECAR_PORT".to_owned(), port.to_string().into()),
        ("PMT_SIDECAR_TOKEN".to_owned(), token.into()),
    ];
    if let Some(path) = std::env::var_os("PATH") {
        env.push(("PATH".to_owned(), path));
    }
    if spec.declaration.runtime == Runtime::Node {
        env.push((
            "NODE_PATH".to_owned(),
            modules_dir(&spec.home, &spec.plugin).into(),
        ));
    }
    env.extend(spec.secrets.iter().map(|(k, v)| (k.clone(), v.into())));
    env
}

async fn wait_ready(
    port: u16,
    limit: Duration,
    child: &mut tokio::process::Child,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return Ok(());
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("the sidecar exited before serving: {status}"));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "the sidecar did not serve port {port} within {}s",
                limit.as_secs()
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn publish(path: &Path, port: u16, token: &str, pid: Option<u32>) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let record = json!({"port": port, "token": token, "pid": pid});
    pmt_core::private_fs::write_atomic(path, record.to_string().as_bytes())
}

/// Waits for the process: a wanted stop kills it quietly, an exit of its
/// own is the adapter's `retrying`.
async fn watch(
    mut child: tokio::process::Child,
    stopped: oneshot::Receiver<()>,
    link: Arc<dyn AdapterLink>,
    plugin: String,
) {
    tokio::select! {
        status = child.wait() => {
            let status = status.map_or_else(|e| e.to_string(), |s| s.to_string());
            tracing::warn!(plugin, "the adapter's sidecar exited: {status}");
            link.report(AdapterState::Retrying, Some(AdapterError {
                retryable: true,
                code: EXITED.to_owned(),
                message: format!("the sidecar of plugin '{plugin}' exited ({status})"),
            }));
        }
        _ = stopped => {
            let _ = child.kill().await;
        }
    }
}

/// Where a Node sidecar's packages are installed.
pub(crate) fn modules_dir(home: &Path, plugin: &str) -> PathBuf {
    home.join("runtimes")
        .join("sidecars")
        .join(plugin)
        .join("node_modules")
}

/// The npm CLI shipped next to the managed `node`.
fn npm_cli(node: &Path) -> Option<PathBuf> {
    let bin = node.parent()?;
    [
        bin.parent()?.join("lib/node_modules/npm/bin/npm-cli.js"),
        bin.join("node_modules/npm/bin/npm-cli.js"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

/// Installs a Node sidecar's packages from its lockfile with `npm ci`
/// (every package checked against the lockfile's integrity hash), once per
/// lockfile digest.
///
/// # Errors
///
/// The lockfile cannot be read or `npm ci` failed.
pub(crate) fn install_packages(spec: &Spec, lockfile: &str, node: &Path) -> Result<(), String> {
    use sha2::Digest as _;
    let lock = spec.plugin_dir.join(lockfile);
    let manifest = lock.with_file_name("package.json");
    let lock_text = std::fs::read(&lock).map_err(|e| format!("{}: {e}", lock.display()))?;
    let package = std::fs::read(&manifest).map_err(|e| format!("{}: {e}", manifest.display()))?;
    let digest = hex(&sha2::Sha256::digest(
        [lock_text.as_slice(), package.as_slice()].concat(),
    ));
    let dir = spec
        .home
        .join("runtimes")
        .join("sidecars")
        .join(&spec.plugin);
    let marker = dir.join(".prometheus-sidecar");
    if std::fs::read_to_string(&marker).is_ok_and(|m| m == digest) {
        return Ok(());
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for (name, bytes) in [
        ("package-lock.json", &lock_text),
        ("package.json", &package),
    ] {
        std::fs::write(dir.join(name), bytes).map_err(|e| format!("{name}: {e}"))?;
    }
    let npm = npm_cli(node).ok_or_else(|| format!("no npm next to {}", node.display()))?;
    let out = std::process::Command::new(node)
        .arg(&npm)
        .args([
            "ci",
            "--omit=dev",
            "--ignore-scripts",
            "--no-audit",
            "--no-fund",
        ])
        .current_dir(&dir)
        .output()
        .map_err(|e| format!("npm ci: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "npm ci failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    std::fs::write(&marker, digest).map_err(|e| format!("{}: {e}", marker.display()))
}

/// The managed Node runtime (installed when missing) with the sidecar's
/// packages installed.
pub(crate) async fn prepare_node(
    spec: &Spec,
    network: &pmt_config::schema::NetworkConfig,
) -> Result<PathBuf, AdapterError> {
    let root = spec.home.clone();
    let policy = crate::runtimes_cmd::browser::outbound(network);
    let config =
        pmt_config::DaemonConfig::load(&root).map_err(|e| failed(e.message().to_owned()))?;
    let store = pmt_update::runtimes::Store::open(&root);
    let progress: pmt_update::runtimes::install::Progress =
        Arc::new(|line| tracing::info!("{line}"));
    let installer = crate::runtimes_cmd::browser::installer(store, &config, &policy, progress);
    let node = installer
        .install("node")
        .await
        .map_err(|e| failed(e.message().to_owned()))?
        .exe();
    if let Some(lockfile) = spec.declaration.lockfile.clone() {
        let (spec, node) = (spec.clone(), node.clone());
        pmt_core::spawn::blocking(move || install_packages(&spec, &lockfile, &node))
            .await
            .map_err(|e| failed(e.to_string()))?
            .map_err(failed)?;
    }
    Ok(node)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use pmt_gateway::messaging::{AdapterFuture, Admission, Inbound};

    use super::*;

    #[derive(Default)]
    struct Link(Mutex<Vec<(AdapterState, Option<String>)>>);

    impl AdapterLink for Link {
        fn submit(&self, _: Inbound) -> AdapterFuture<'_, Admission> {
            Box::pin(async { Admission::Accepted })
        }

        fn report(&self, state: AdapterState, error: Option<AdapterError>) {
            self.0.lock().unwrap().push((state, error.map(|e| e.code)));
        }
    }

    /// A plugin whose sidecar is the MCP fixture server serving HTTP on
    /// the given port.
    fn spec(home: &Path) -> Spec {
        let plugin_dir = home.join("plugins").join("photon");
        std::fs::create_dir_all(plugin_dir.join("bin")).unwrap();
        let exe = format!("side{}", std::env::consts::EXE_SUFFIX);
        std::fs::copy(
            pmt_mcp::testkit::fixture(),
            plugin_dir.join("bin").join(&exe),
        )
        .unwrap();
        Spec {
            home: home.to_path_buf(),
            profile: "default".into(),
            plugin: "photon".into(),
            plugin_dir,
            declaration: SidecarDeclaration {
                runtime: Runtime::None,
                command: format!("bin/{exe}"),
                args: vec!["--http".into(), "127.0.0.1:{port}".into()],
                lockfile: None,
                secrets: vec!["PHOTON_TOKEN".into()],
                ready_timeout_secs: 30,
            },
            secrets: vec![("PHOTON_TOKEN".into(), "t0k".into())],
            node: None,
        }
    }

    #[test]
    fn the_environment_holds_only_the_port_token_path_and_granted_secrets() {
        let home =
            tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")).unwrap();
        let names: Vec<String> = environment(&spec(home.path()), 4000, "tok")
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(
            names,
            [
                "PMT_SIDECAR_PORT",
                "PMT_SIDECAR_TOKEN",
                "PATH",
                "PHOTON_TOKEN"
            ]
        );
        let declared = SidecarDeclaration {
            secrets: vec!["A".into(), "B".into()],
            ..spec(home.path()).declaration
        };
        let grants = CapabilitySet::parse(["secrets.read:A"]).unwrap();
        assert_eq!(granted_secrets(&declared, &grants), ["A"]);
    }

    /// Spec: platform-adapters, Supervised sidecar adapters, "Sidecar
    /// crash contained".
    #[tokio::test]
    async fn a_sidecar_exit_puts_only_its_adapter_into_retrying() {
        let home =
            tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")).unwrap();
        let spec = spec(home.path());
        let link = Arc::new(Link::default());
        let running = start(&spec, Arc::clone(&link) as Arc<dyn AdapterLink>)
            .await
            .unwrap();
        let file = run_file(home.path(), "default", "photon");
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(record["port"], running.port);
        assert_eq!(record["token"], running.token.as_str());
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", running.port))
                .await
                .is_ok()
        );
        let pid = record["pid"].as_u64().unwrap().to_string();
        #[cfg(unix)]
        let killed = std::process::Command::new("kill")
            .args(["-9", &pid])
            .status();
        #[cfg(windows)]
        let killed = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid])
            .status();
        assert!(killed.unwrap().success());
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while link.0.lock().unwrap().is_empty() {
            assert!(tokio::time::Instant::now() < deadline, "no report");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            *link.0.lock().unwrap(),
            [(AdapterState::Retrying, Some(EXITED.to_owned()))]
        );
        drop(running);
        assert!(!file.exists(), "the run file goes with the sidecar");
    }

    #[tokio::test]
    async fn a_wanted_stop_reports_nothing() {
        let home =
            tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")).unwrap();
        let link = Arc::new(Link::default());
        let running = start(
            &spec(home.path()),
            Arc::clone(&link) as Arc<dyn AdapterLink>,
        )
        .await
        .unwrap();
        let port = running.port;
        drop(running);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(link.0.lock().unwrap().is_empty());
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err()
        );
    }
}
