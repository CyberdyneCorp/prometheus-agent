//! Container lifecycle (spec: execution-environments, Container lifecycle
//! and orphan reaping; Container hardening is never silently dropped).
//!
//! A [`Container`] is created on first use with every hardening flag, and a
//! flag the runtime rejects either fails creation naming it or, when
//! `allow_degraded` waives it, is dropped with a WARN on every start. One
//! process-wide [`Pool`] hands sessions that resolve to the same container
//! name (see [`ContainerSpec::name`]) the same handle. The pool's reaper
//! tears a container down once every session using it has been idle past
//! `terminal.idle_lifetime_secs` with nothing in flight; persistent
//! containers are stopped, the others removed, and the next command starts
//! them again. [`reap_orphans`] runs once per gateway start and spares the
//! stopped persistent containers: they are reused, not orphans.
//!
//! A container records how it was made in two labels: [`SPEC_LABEL`], a
//! digest of its `create` arguments, and [`DROPPED_LABEL`], the hardening
//! flags dropped under `allow_degraded`. An existing container is reused
//! only when it was made from the current settings and every flag it lacks
//! is still waived (then the WARN is repeated on this start, even in a new
//! process); otherwise it is removed and created again, so a reused
//! container never runs with less hardening, or other settings, than the
//! configuration asks for.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};
use std::time::Duration;

use pmt_config::schema::{ContainerConfig, DegradedFeature};
use pmt_core::error::{Error, codes};
use sha2::{Digest, Sha256};

use super::hardening::{self, Flag};
use super::options::Volume;
use super::{Runtime, first_line};

/// The label naming the owning profile.
pub const PROFILE_LABEL: &str = "prometheus.profile";
/// The label naming the creating session.
pub const SESSION_LABEL: &str = "prometheus.session";
/// The label marking a persistent container (value `true`).
pub const PERSISTENT_LABEL: &str = "prometheus.persistent";
/// The label holding a digest of the settings the container was created
/// from (every `create` argument but its name and session).
pub const SPEC_LABEL: &str = "prometheus.spec";
/// Where the session workspace is mounted inside a container
/// (`terminal.container.mount_workspace`).
pub const WORKSPACE: &str = "/workspace";

/// The label listing the hardening flags dropped under `allow_degraded`
/// (comma separated, empty when none).
pub const DROPPED_LABEL: &str = "prometheus.dropped";

/// Keeps a container alive: what a session reports to the reaper.
pub trait Activity: Send + Sync {
    /// A foreground call is in flight or a tracked background process is
    /// running.
    fn busy(&self) -> bool;
    /// Time since the last call finished.
    fn idle_for(&self) -> Duration;
}

/// Everything a container is created from.
#[derive(Debug, Clone)]
pub struct ContainerSpec {
    /// The runtime CLI.
    pub runtime: Runtime,
    /// The owning profile id (label).
    pub profile: String,
    /// The creating session id (label).
    pub session: String,
    /// The image.
    pub image: String,
    /// `[terminal.container]`.
    pub config: ContainerConfig,
    /// `security.sandbox.allow_degraded`.
    pub allow_degraded: Vec<DegradedFeature>,
    /// The host workspace mounted at `/workspace` (`mount_workspace`).
    pub workspace: Option<PathBuf>,
    /// Validated `terminal.container.volumes`.
    pub volumes: Vec<Volume>,
    /// `--user <uid>:<gid>` (`run_as_host_user`).
    pub user: Option<(u32, u32)>,
    /// `terminal.idle_lifetime_secs`.
    pub idle_lifetime: Duration,
}

impl ContainerSpec {
    /// The container's name, which decides who shares it: every session
    /// of the profile with the same `shared_key`; else, for a persistent
    /// container, every session of the profile on the same mounted
    /// workspace (or all of them without a workspace mount), so a later
    /// session reuses it; else only this session.
    #[must_use]
    pub fn name(&self) -> String {
        let owner = if !self.config.shared_key.is_empty() {
            format!("k-{}", self.config.shared_key)
        } else if !self.config.persistent {
            format!("s-{}", self.session)
        } else if let Some(workspace) = &self.workspace {
            // A digest keeps the name short and free of path characters;
            // sha256 (not std's hasher) keeps it stable across releases.
            let digest = Sha256::digest(workspace.as_os_str().as_encoded_bytes());
            format!("w-{}", hex(&digest[..8]))
        } else {
            "p".to_owned()
        };
        let clean = |s: &str| -> String {
            s.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect()
        };
        format!("pmt-{}-{}", clean(&self.profile), clean(&owner))
    }

    /// The settings `create` arguments: no name, session, record labels or
    /// hardening flags.
    fn base_args(&self) -> Vec<String> {
        let mut args = vec![
            format!("--label={PROFILE_LABEL}={}", self.profile),
            format!("--shm-size={}", self.config.shm_size),
        ];
        if self.config.persistent {
            args.push(format!("--label={PERSISTENT_LABEL}=true"));
        }
        if !self.config.network {
            args.push("--network=none".to_owned());
        }
        if let Some((uid, gid)) = self.user {
            args.push(format!("--user={uid}:{gid}"));
        }
        args.extend(self.workspace_arg());
        args.extend(self.volumes.iter().map(Volume::arg));
        args.extend(
            self.config
                .env
                .iter()
                .map(|(k, v)| format!("--env={k}={v}")),
        );
        args
    }

    /// The workspace mount (`mount_workspace`).
    fn workspace_arg(&self) -> Option<String> {
        self.workspace
            .as_ref()
            .map(|w| format!("--volume={}:{WORKSPACE}", w.display()))
    }

    /// A digest of everything the container is created from: the settings,
    /// every hardening flag asked for (dropped or not), the extra arguments
    /// and the image. Two specs with one digest create the same container.
    /// A `shared_key` container is shared whatever each session's
    /// workspace is (the first session's stays mounted), so its workspace
    /// mount is left out: a session on another workspace does not replace
    /// the container the key shares.
    fn digest(&self) -> String {
        let mut hasher = Sha256::new();
        let workspace = self.workspace_arg();
        let args = self
            .base_args()
            .into_iter()
            .filter(|a| self.config.shared_key.is_empty() || Some(a) != workspace.as_ref())
            .chain(
                hardening::flags(&self.config)
                    .into_iter()
                    .flat_map(|f| f.args),
            )
            .chain(self.config.extra_args.iter().cloned())
            .chain(self.command_args());
        for arg in args {
            hasher.update(arg.as_bytes());
            hasher.update([0]);
        }
        hex(&hasher.finalize()[..16])
    }

    /// The image and the idle keeper as PID 1 (exits on SIGTERM).
    fn command_args(&self) -> Vec<String> {
        vec![
            "--entrypoint=sh".to_owned(),
            self.image.clone(),
            "-c".to_owned(),
            "trap 'exit 0' TERM INT; while :; do sleep 3600 & wait $!; done".to_owned(),
        ]
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    })
}

#[derive(Debug, Default)]
struct State {
    running: bool,
}

/// An existing container as `inspect` reports it.
struct Existing {
    running: bool,
    /// Its [`SPEC_LABEL`].
    spec: String,
    /// Its [`DROPPED_LABEL`], split.
    dropped: Vec<String>,
}

/// One container and the sessions using it.
pub struct Container {
    spec: ContainerSpec,
    name: String,
    state: Mutex<State>,
    users: Mutex<Vec<(usize, Weak<dyn Activity>)>>,
    next_user: std::sync::atomic::AtomicUsize,
}

impl std::fmt::Debug for Container {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Container")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Container {
    /// Its name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The runtime it runs on.
    #[must_use]
    pub fn runtime(&self) -> &Runtime {
        &self.spec.runtime
    }

    /// Its spec.
    #[must_use]
    pub fn spec(&self) -> &ContainerSpec {
        &self.spec
    }

    /// Registers a session using the container (the reaper asks it before
    /// tearing down); the id releases it.
    pub fn attach(&self, user: Weak<dyn Activity>) -> usize {
        let id = self
            .next_user
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.users
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((id, user));
        id
    }

    /// Makes sure the container is running: an existing one made from the
    /// current settings is started (or used when running), anything else
    /// is created with every hardening flag. Each dropped flag is logged
    /// at WARN whenever this process starts using the container. Blocks on
    /// the runtime.
    ///
    /// # Errors
    ///
    /// `terminal.container_hardening` for a rejected flag no
    /// `allow_degraded` entry waives (no container is started);
    /// `terminal.container_runtime` when the runtime fails.
    pub fn ensure_running(&self) -> Result<(), Error> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.running {
            return Ok(());
        }
        let digest = self.spec.digest();
        let dropped = match self.inspect()? {
            Some(existing) if self.reusable(&existing, &digest) => {
                if !existing.running {
                    self.start()?;
                }
                existing.dropped
            }
            outdated => {
                if outdated.is_some() {
                    tracing::info!(
                        container = %self.name,
                        "recreating container: it was made from other settings or with hardening no longer waived"
                    );
                    self.remove()?;
                }
                let dropped = self.create(&digest)?;
                self.start()?;
                dropped
            }
        };
        for flag in &dropped {
            tracing::warn!(
                container = %self.name,
                "container hardening '{flag}' unsupported by runtime; running without it (security.sandbox.allow_degraded)"
            );
        }
        state.running = true;
        Ok(())
    }

    /// Whether `existing` was made from these settings and lacks only
    /// flags `allow_degraded` still waives.
    fn reusable(&self, existing: &Existing, digest: &str) -> bool {
        let flags = hardening::flags(&self.spec.config);
        existing.spec == digest
            && existing.dropped.iter().all(|name| {
                flags
                    .iter()
                    .any(|f| f.name == name && f.waived_by(&self.spec.allow_degraded))
            })
    }

    /// The existing container, `None` when missing.
    fn inspect(&self) -> Result<Option<Existing>, Error> {
        let runtime = &self.spec.runtime;
        let output = runtime.run(&[
            "inspect".into(),
            "--type=container".into(),
            format!("--format={{{{.State.Running}}}}|{{{{index .Config.Labels \"{SPEC_LABEL}\"}}}}|{{{{index .Config.Labels \"{DROPPED_LABEL}\"}}}}"),
            self.name.clone(),
        ])?;
        if !output.status.success() {
            return Ok(None);
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut parts = text.trim().splitn(3, '|');
        let running = parts.next() == Some("true");
        // A missing label prints `<no value>` (Docker) or nothing (Podman):
        // neither matches a digest, so such a container is recreated.
        let spec = parts.next().unwrap_or_default().to_owned();
        let dropped = parts
            .next()
            .unwrap_or_default()
            .split(',')
            .filter(|f| !f.is_empty() && *f != "<no value>")
            .map(str::to_owned)
            .collect();
        Ok(Some(Existing {
            running,
            spec,
            dropped,
        }))
    }

    /// Creates the container, dropping only waived flags the runtime
    /// rejects; returns the dropped flags.
    fn create(&self, digest: &str) -> Result<Vec<String>, Error> {
        super::options::extra_args(&self.spec.config.extra_args, &self.spec.allow_degraded)?;
        let mut active: Vec<Flag> = hardening::flags(&self.spec.config);
        let mut dropped: Vec<String> = Vec::new();
        loop {
            let mut args = vec![
                "create".to_owned(),
                format!("--name={}", self.name),
                format!("--label={SESSION_LABEL}={}", self.spec.session),
                format!("--label={SPEC_LABEL}={digest}"),
                format!("--label={DROPPED_LABEL}={}", dropped.join(",")),
            ];
            args.extend(self.spec.base_args());
            args.extend(active.iter().flat_map(|f| f.args.iter().cloned()));
            args.extend(self.spec.config.extra_args.iter().cloned());
            args.extend(self.spec.command_args());
            let output = self.spec.runtime.run(&args)?;
            if output.status.success() {
                return Ok(dropped);
            }
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let Some(index) = hardening::rejected(&active, &stderr)
                .and_then(|flag| active.iter().position(|f| f.name == flag.name))
            else {
                return Err(Error::new(
                    &codes::TERMINAL_CONTAINER_RUNTIME,
                    format!(
                        "{} create failed: {}",
                        self.spec.runtime.name(),
                        first_line(stderr.as_bytes())
                    ),
                ));
            };
            let flag = active.remove(index);
            if !flag.waived_by(&self.spec.allow_degraded) {
                return Err(hardening::unsupported(&flag));
            }
            dropped.push(flag.name.to_owned());
        }
    }

    fn start(&self) -> Result<(), Error> {
        self.run_checked("start", &["start".into(), self.name.clone()])
    }

    /// Removes the existing container (running or not).
    fn remove(&self) -> Result<(), Error> {
        self.run_checked("rm", &["rm".into(), "--force".into(), self.name.clone()])
    }

    fn run_checked(&self, verb: &str, args: &[String]) -> Result<(), Error> {
        let output = self.spec.runtime.run(args)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(Error::new(
                &codes::TERMINAL_CONTAINER_RUNTIME,
                format!(
                    "{} {verb} failed: {}",
                    self.spec.runtime.name(),
                    first_line(&output.stderr)
                ),
            ))
        }
    }

    /// Forgets that the container runs (an exec failed): the next
    /// [`Self::ensure_running`] inspects it again.
    pub fn evict(&self) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .running = false;
    }

    /// Stops a persistent container, removes any other. Blocks.
    pub fn teardown(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let args: Vec<String> = if self.spec.config.persistent {
            vec!["stop".into(), "--time=2".into(), self.name.clone()]
        } else {
            vec!["rm".into(), "--force".into(), self.name.clone()]
        };
        if let Err(err) = self.spec.runtime.run(&args) {
            tracing::warn!(container = %self.name, "teardown failed: {}", err.message());
        }
        state.running = false;
    }

    /// Whether every live session using it is idle past the lifetime.
    fn idle(&self) -> bool {
        let mut users = self.users.lock().unwrap_or_else(PoisonError::into_inner);
        users.retain(|(_, u)| u.strong_count() > 0);
        users
            .iter()
            .filter_map(|(_, u)| u.upgrade())
            .all(|user| !user.busy() && user.idle_for() >= self.spec.idle_lifetime)
    }

    /// Session end of the user `id` (from [`Self::attach`]; `None` for a
    /// user that never attached): when no other live session uses the
    /// container it is torn down at once.
    pub fn release(&self, id: Option<usize>) {
        let others = {
            let mut users = self.users.lock().unwrap_or_else(PoisonError::into_inner);
            users.retain(|(uid, u)| Some(*uid) != id && u.strong_count() > 0);
            users.len()
        };
        if others == 0 {
            self.teardown();
        }
    }

    fn reap_if_idle(&self) {
        let running = self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .running;
        if running && self.idle() {
            tracing::info!(container = %self.name, "tearing down idle container");
            self.teardown();
        }
    }
}

/// The process-wide containers, by name.
pub struct Pool {
    containers: Mutex<HashMap<String, Weak<Container>>>,
}

static POOL: LazyLock<Arc<Pool>> = LazyLock::new(|| {
    let pool = Arc::new(Pool {
        containers: Mutex::new(HashMap::new()),
    });
    let weak = Arc::downgrade(&pool);
    std::thread::Builder::new()
        .name("pmt-container-reaper".into())
        .spawn(move || reaper(&weak))
        .ok();
    pool
});

/// How often the reaper looks at idle containers.
const REAP_TICK: Duration = Duration::from_millis(500);

fn reaper(pool: &Weak<Pool>) {
    loop {
        std::thread::sleep(REAP_TICK);
        let Some(pool) = pool.upgrade() else { return };
        let live: Vec<Arc<Container>> = {
            let mut map = pool
                .containers
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            map.retain(|_, c| c.strong_count() > 0);
            map.values().filter_map(Weak::upgrade).collect()
        };
        for container in live {
            container.reap_if_idle();
        }
    }
}

impl Pool {
    /// The process-wide pool (its reaper thread starts on first use).
    #[must_use]
    pub fn process() -> Arc<Self> {
        Arc::clone(&POOL)
    }

    /// The container `spec` names, shared with every session already
    /// using it with the same settings and waivers. A live handle made
    /// from other settings or other `allow_degraded` entries is replaced:
    /// its in-memory "running" state would otherwise let this session skip
    /// the label check and run without hardening it no longer waives.
    #[must_use]
    pub fn container(&self, spec: ContainerSpec) -> Arc<Container> {
        let name = spec.name();
        let mut map = self
            .containers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(existing) = map.get(&name).and_then(Weak::upgrade)
            && existing.spec.allow_degraded == spec.allow_degraded
            && existing.spec.digest() == spec.digest()
        {
            return existing;
        }
        let container = Arc::new(Container {
            spec,
            name: name.clone(),
            state: Mutex::new(State::default()),
            users: Mutex::new(Vec::new()),
            next_user: std::sync::atomic::AtomicUsize::new(0),
        });
        map.insert(name, Arc::downgrade(&container));
        container
    }
}

/// Removes `profile`'s exited containers that finished more than twice
/// `idle_lifetime` ago, and nothing else: another profile's containers,
/// running ones and persistent ones (stopped on purpose, started again by
/// the next session on their workspace) are left alone. Returns the
/// removed ids.
///
/// # Errors
///
/// `terminal.container_runtime` when the runtime cannot list them.
pub fn reap_orphans(
    runtime: &Runtime,
    profile: &str,
    idle_lifetime: Duration,
) -> Result<Vec<String>, Error> {
    let listed = runtime.run(&[
        "ps".into(),
        "--all".into(),
        format!("--filter=label={PROFILE_LABEL}={profile}"),
        "--filter=status=exited".into(),
        "--format={{.ID}}".into(),
    ])?;
    let now = jiff::Timestamp::now();
    let mut removed = Vec::new();
    for id in String::from_utf8_lossy(&listed.stdout).split_whitespace() {
        let inspected = runtime.run(&[
            "inspect".into(),
            "--type=container".into(),
            format!("--format={{{{index .Config.Labels \"{PROFILE_LABEL}\"}}}}|{{{{.State.Running}}}}|{{{{.State.FinishedAt}}}}|{{{{index .Config.Labels \"{PERSISTENT_LABEL}\"}}}}"),
            id.to_owned(),
        ])?;
        let text = String::from_utf8_lossy(&inspected.stdout).trim().to_owned();
        let mut parts = text.splitn(4, '|');
        let (Some(label), Some(running), Some(finished)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if parts.next() == Some("true") {
            continue;
        }
        let old_enough = finished_at(finished).is_some_and(|at| {
            now.duration_since(at).as_secs_f64() > 2.0 * idle_lifetime.as_secs_f64()
        });
        if label == profile && running == "false" && old_enough {
            let output = runtime.run(&["rm".into(), id.to_owned()])?;
            if output.status.success() {
                removed.push(id.to_owned());
            }
        }
    }
    Ok(removed)
}

/// `FinishedAt` as Docker (RFC 3339) or Podman (Go's default layout,
/// `2006-01-02 15:04:05.999 -0700 MST`) prints it.
fn finished_at(text: &str) -> Option<jiff::Timestamp> {
    if let Ok(at) = text.parse::<jiff::Timestamp>() {
        return Some(at);
    }
    let mut fields = text.split_whitespace();
    let go = format!("{} {} {}", fields.next()?, fields.next()?, fields.next()?);
    jiff::fmt::strtime::parse("%Y-%m-%d %H:%M:%S%.f %z", &go)
        .ok()?
        .to_timestamp()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::finished_at;

    #[test]
    fn both_runtimes_finish_times_parse() {
        let docker = finished_at("2026-01-02T03:04:05.123456789Z").unwrap();
        let podman = finished_at("2026-01-02 03:04:05.123456789 +0000 UTC").unwrap();
        assert_eq!(docker, podman);
        assert!(finished_at("0001-01-01T00:00:00Z").is_some());
        assert!(finished_at("garbage").is_none());
    }
}
