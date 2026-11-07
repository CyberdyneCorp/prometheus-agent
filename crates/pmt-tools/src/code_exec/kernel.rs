//! The persistent `execute_code` kernel (spec: subagent-delegation,
//! Persistent `execute_code` session kernel).
//!
//! One Python process per (session, mode, interpreter, working directory,
//! stub tools) runs every cell of that session in one namespace, so
//! imports and variables persist between calls. Its environment is fixed
//! when it starts (the same scrubbed environment a one-shot script gets);
//! the stub tools a cell may call are checked again on every cell by a
//! fresh channel server. A cell that times out or is interrupted kills
//! the kernel, and the result says its state was lost. Kernels idle for
//! `code_execution.kernel_idle_timeout_secs` are stopped by a sweeper,
//! and beyond `code_execution.max_session_kernels` top-level kernels the
//! least recently used one is stopped (children's kernels are not
//! counted). A session's kernels stop when it ends.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, Once, PoisonError};
use std::time::{Duration, Instant};

use pmt_core::cancel::CancelToken;
use pmt_core::error::{Error, codes};
use pmt_sandbox::Sandbox;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

use super::channel::{Server, StubCalls, Token};
use super::output::{self, Ran};
use crate::ToolResult;

/// The kernel's driver program.
const DRIVER: &str = include_str!("driver.py");

/// What the result says when a cell's kernel was stopped or died.
pub const STATE_LOST: &str = "The Python kernel was stopped; its imports and variables were lost. The next call starts a fresh one.";

/// How often idle kernels are looked for.
const SWEEP_EVERY: Duration = Duration::from_secs(1);

/// What one kernel is shared by.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    /// The session.
    pub session: String,
    /// `code_execution.mode`.
    pub mode: &'static str,
    /// The interpreter.
    pub interpreter: PathBuf,
    /// The session's working directory.
    pub workdir: PathBuf,
    /// The stub tools the kernel's module offers.
    pub tools: Vec<String>,
}

/// Where a kernel runs.
pub struct Placement<'a> {
    /// The session's sandbox.
    pub sandbox: &'a Sandbox,
    /// A directory the session may write (the kernel's files go below).
    pub scratch: &'a Path,
    /// Run in a fresh staging directory instead of the working directory.
    pub staging: bool,
    /// A top-level session's kernel (children's are not counted).
    pub top_level: bool,
    /// Stop it after this long unused.
    pub idle: Duration,
    /// Top-level kernels kept at once.
    pub max_top_level: usize,
}

/// One cell.
pub struct Cell<'a> {
    /// The Python source.
    pub code: &'a str,
    /// Stub calls allowed in this cell.
    pub max_tool_calls: u32,
    /// Wall-clock limit of this cell.
    pub timeout: Duration,
    /// Where the full stdout is kept when it is capped.
    pub spill_dir: &'a Path,
    /// Discard the kernel's state first.
    pub reset: bool,
}

/// Runs `cell` in the kernel of `key`, starting one when needed;
/// `cancel` (the turn's hard stop) kills it.
pub async fn run(
    key: Key,
    place: &Placement<'_>,
    cell: &Cell<'_>,
    calls: &dyn StubCalls,
    cancel: &CancelToken,
) -> ToolResult {
    if cell.code.trim().is_empty() {
        return ToolResult::error(super::NO_CODE);
    }
    let slot = match POOL.checkout(&key, place, cell.reset) {
        Ok(slot) => slot,
        Err(err) => return super::refusal(&err),
    };
    let mut kernel = slot.lock().await;
    let ran = kernel.execute(cell, &key.tools, calls, cancel).await;
    drop(kernel);
    let lost = !matches!(ran.ended, Ended::Exited(_));
    if lost {
        POOL.discard(&key, &slot);
    } else {
        POOL.touch(&key);
    }
    result(&ran, cell)
}

/// Stops every kernel of `session` (it ended).
pub fn close_session(session: &str) {
    let removed: Vec<Slot> = {
        let mut slots = POOL.lock();
        let keys: Vec<Key> = slots
            .keys()
            .filter(|k| k.session == session)
            .cloned()
            .collect();
        keys.iter().filter_map(|k| slots.remove(k)).collect()
    };
    drop(removed);
}

/// The result of a cell.
fn result(ran: &Executed, cell: &Cell<'_>) -> ToolResult {
    let exit_code = match ran.ended {
        Ended::Exited(code) => code,
        _ => None,
    };
    let mut body = output::body(&Ran {
        stdout: &ran.stdout,
        stderr: &ran.stderr,
        exit_code,
        timed_out: matches!(ran.ended, Ended::TimedOut),
        cancelled: matches!(ran.ended, Ended::Cancelled),
        tool_calls: ran.tool_calls,
        timeout: cell.timeout,
        spill_dir: cell.spill_dir,
    });
    if matches!(ran.ended, Ended::Died) {
        body["error"] = "The Python kernel exited during the call.".into();
    }
    if !matches!(ran.ended, Ended::Exited(_)) {
        body["state_lost"] = true.into();
        body["kernel"] = STATE_LOST.into();
    }
    output::finish(&body, exit_code == Some(0))
}

/// How a cell ended.
#[derive(Debug, Clone, Copy)]
enum Ended {
    /// It finished with this exit code (the kernel lives on).
    Exited(Option<i32>),
    /// The time limit stopped it.
    TimedOut,
    /// The turn's stop interrupted it.
    Cancelled,
    /// The kernel process died.
    Died,
}

struct Executed {
    ended: Ended,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    tool_calls: u32,
}

/// A running kernel process and its private directory.
struct Kernel {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    dir: PathBuf,
    rpc: PathBuf,
    cells: PathBuf,
    token: Token,
    next: u64,
}

impl Kernel {
    fn start(key: &Key, place: &Placement<'_>) -> Result<Self, Error> {
        let dir = place
            .scratch
            .join(format!("kernel-{}", ulid::Ulid::generate()));
        let io = |e: std::io::Error| Error::io(&codes::HOME_IO, &dir, &e);
        let (home, rpc, cells) = (dir.join("home"), dir.join("rpc"), dir.join("cells"));
        for sub in [&home, &rpc, &cells] {
            std::fs::create_dir_all(sub).map_err(io)?;
        }
        std::fs::write(
            dir.join("prometheus_tools.py"),
            super::stubs::module(&key.tools),
        )
        .map_err(io)?;
        let driver = dir.join("driver.py");
        std::fs::write(&driver, DRIVER).map_err(io)?;
        let workdir = if place.staging {
            let work = dir.join("work");
            std::fs::create_dir_all(&work).map_err(io)?;
            work
        } else {
            key.workdir.clone()
        };
        let token = Token::new();
        let mut command = place.sandbox.command(&key.interpreter)?;
        command
            .args(["-E", "-s"])
            .arg(&driver)
            .arg(&cells)
            .current_dir(&workdir)
            .env_clear()
            .envs(super::environment(&home, &rpc, &token))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut command = tokio::process::Command::from(command);
        command.kill_on_drop(true);
        let mut child = command.spawn().map_err(|e| {
            Error::new(
                &codes::EXECUTE_CODE_REFUSED,
                format!("execute_code could not start Python: {e}"),
            )
        })?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        Ok(Self {
            child,
            stdin,
            stdout,
            dir,
            rpc,
            cells,
            token,
            next: 0,
        })
    }

    /// Runs one cell, serving its stub calls through `calls`.
    async fn execute(
        &mut self,
        cell: &Cell<'_>,
        tools: &[String],
        calls: &dyn StubCalls,
        cancel: &CancelToken,
    ) -> Executed {
        self.next += 1;
        let number = format!("{:08}", self.next);
        let base = self.cells.join(&number);
        let server = Server::new(
            self.rpc.clone(),
            self.token.clone(),
            tools.to_vec(),
            cell.max_tool_calls,
        );
        let ended = if self.send(&base, &number, cell.code).await {
            self.wait(cell.timeout, &server, calls, cancel).await
        } else {
            Ended::Died
        };
        let stdout = std::fs::read(base.with_extension("out")).unwrap_or_default();
        let stderr = std::fs::read(base.with_extension("err")).unwrap_or_default();
        for ext in ["py", "out", "err"] {
            let _ = std::fs::remove_file(base.with_extension(ext));
        }
        Executed {
            ended,
            stdout,
            stderr,
            tool_calls: server.calls(),
        }
    }

    /// Hands cell `number` to the kernel; `false` when it is gone.
    async fn send(&mut self, base: &Path, number: &str, code: &str) -> bool {
        if std::fs::write(base.with_extension("py"), code).is_err() {
            return false;
        }
        let line = format!("{number}\n");
        self.stdin.write_all(line.as_bytes()).await.is_ok() && self.stdin.flush().await.is_ok()
    }

    async fn wait(
        &mut self,
        timeout: Duration,
        server: &Server,
        calls: &dyn StubCalls,
        cancel: &CancelToken,
    ) -> Ended {
        let mut line = String::new();
        tokio::select! {
            read = self.stdout.read_line(&mut line) => match read {
                Ok(n) if n > 0 => Ended::Exited(exit_code(&line)),
                _ => Ended::Died,
            },
            () = tokio::time::sleep(timeout) => Ended::TimedOut,
            () = cancel.cancelled() => Ended::Cancelled,
            // The channel serves until the cell ends; it never returns first.
            () = server.serve(calls) => Ended::Died,
        }
    }
}

impl Drop for Kernel {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The exit code a driver's report line carries.
fn exit_code(line: &str) -> Option<i32> {
    serde_json::from_str::<Value>(line)
        .ok()?
        .get("exit_code")?
        .as_i64()
        .and_then(|c| i32::try_from(c).ok())
}

/// One pooled kernel.
struct Slot {
    kernel: Arc<tokio::sync::Mutex<Kernel>>,
    top_level: bool,
    idle: Duration,
    last_used: Instant,
}

/// Every kernel of the process.
struct Pool {
    slots: Mutex<HashMap<Key, Slot>>,
    sweeper: Once,
}

static POOL: LazyLock<Pool> = LazyLock::new(|| Pool {
    slots: Mutex::default(),
    sweeper: Once::new(),
});

type Shared = Arc<tokio::sync::Mutex<Kernel>>;

impl Pool {
    fn lock(&self) -> MutexGuard<'_, HashMap<Key, Slot>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The kernel of `key` (a new one after `reset` or when none lives).
    fn checkout(&self, key: &Key, place: &Placement<'_>, reset: bool) -> Result<Shared, Error> {
        self.sweeper.call_once(|| {
            std::thread::spawn(|| {
                loop {
                    std::thread::sleep(SWEEP_EVERY);
                    POOL.reap(Instant::now());
                }
            });
        });
        let mut dropped = Vec::new();
        let shared = {
            let mut slots = self.lock();
            if reset {
                dropped.extend(slots.remove(key));
            }
            if let Some(slot) = slots.get_mut(key) {
                slot.last_used = Instant::now();
                Arc::clone(&slot.kernel)
            } else {
                let kernel = Arc::new(tokio::sync::Mutex::new(Kernel::start(key, place)?));
                slots.insert(
                    key.clone(),
                    Slot {
                        kernel: Arc::clone(&kernel),
                        top_level: place.top_level,
                        idle: place.idle,
                        last_used: Instant::now(),
                    },
                );
                dropped.extend(evict_beyond(&mut slots, place.max_top_level, key));
                kernel
            }
        };
        drop(dropped);
        Ok(shared)
    }

    fn touch(&self, key: &Key) {
        if let Some(slot) = self.lock().get_mut(key) {
            slot.last_used = Instant::now();
        }
    }

    /// Forgets `key`'s kernel if it is still `kernel` (it was stopped).
    fn discard(&self, key: &Key, kernel: &Shared) {
        let removed = {
            let mut slots = self.lock();
            match slots.get(key) {
                Some(slot) if Arc::ptr_eq(&slot.kernel, kernel) => slots.remove(key),
                _ => None,
            }
        };
        drop(removed);
    }

    /// Stops kernels unused for longer than their idle timeout.
    fn reap(&self, now: Instant) {
        let removed: Vec<Slot> = {
            let mut slots = self.lock();
            let idle = idle_keys(&facts(&slots), now);
            idle.iter().filter_map(|k| slots.remove(k)).collect()
        };
        for slot in &removed {
            tracing::debug!(
                idle_secs = slot.idle.as_secs(),
                "idle execute_code kernel stopped"
            );
        }
    }
}

/// What the pool's policies look at for one kernel.
#[derive(Debug, Clone)]
struct Facts {
    key: Key,
    top_level: bool,
    last_used: Instant,
    idle: Duration,
    /// It runs a cell now (never stopped by a policy).
    busy: bool,
}

fn facts(slots: &HashMap<Key, Slot>) -> Vec<Facts> {
    slots
        .iter()
        .map(|(key, s)| Facts {
            key: key.clone(),
            top_level: s.top_level,
            last_used: s.last_used,
            idle: s.idle,
            busy: s.kernel.try_lock().is_err(),
        })
        .collect()
}

/// Kernels unused for at least their idle timeout.
fn idle_keys(kernels: &[Facts], now: Instant) -> Vec<Key> {
    kernels
        .iter()
        .filter(|k| !k.busy && now.saturating_duration_since(k.last_used) >= k.idle)
        .map(|k| k.key.clone())
        .collect()
}

/// The least recently used top-level kernels to stop so at most `max`
/// stay (never `keep`; children's kernels are neither counted nor
/// stopped).
fn evictions(kernels: &[Facts], max: usize, keep: &Key) -> Vec<Key> {
    let top = kernels.iter().filter(|k| k.top_level).count();
    let mut candidates: Vec<&Facts> = kernels
        .iter()
        .filter(|k| k.top_level && !k.busy && &k.key != keep)
        .collect();
    candidates.sort_by_key(|k| k.last_used);
    candidates
        .into_iter()
        .take(top.saturating_sub(max))
        .map(|k| k.key.clone())
        .collect()
}

/// Removes what [`evictions`] picks.
fn evict_beyond(slots: &mut HashMap<Key, Slot>, max: usize, keep: &Key) -> Vec<Slot> {
    let picked = evictions(&facts(slots), max, keep);
    picked.iter().filter_map(|k| slots.remove(k)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kernel(session: &str, top_level: bool, age_secs: u64, now: Instant) -> Facts {
        Facts {
            key: Key {
                session: session.to_owned(),
                mode: "project",
                interpreter: PathBuf::from("python3"),
                workdir: PathBuf::from("."),
                tools: Vec::new(),
            },
            top_level,
            last_used: now.checked_sub(Duration::from_secs(age_secs)).unwrap(),
            idle: Duration::from_mins(30),
            busy: false,
        }
    }

    #[test]
    fn the_least_recently_used_top_level_kernel_goes_and_children_are_not_counted() {
        let now = Instant::now() + Duration::from_secs(10_000);
        let mut kernels: Vec<Facts> = (0..5)
            .map(|i| kernel(&format!("ses-{i}"), true, 100 - i * 10, now))
            .collect();
        kernels.push(kernel("child", false, 1_000, now));
        let newest = kernels[4].key.clone();
        let gone = evictions(&kernels, 4, &newest);
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].session, "ses-0", "the oldest top-level one");
        assert!(evictions(&kernels[..4], 4, &newest).is_empty());
        // A busy kernel is never stopped.
        kernels[0].busy = true;
        assert_eq!(evictions(&kernels, 4, &newest)[0].session, "ses-1");
    }

    #[test]
    fn a_kernel_idle_past_its_timeout_is_reaped() {
        let now = Instant::now() + Duration::from_secs(10_000);
        let kernels = vec![
            kernel("fresh", true, 10, now),
            kernel("stale", true, 1_800, now),
            kernel("stale-child", false, 5_000, now),
        ];
        let reaped: Vec<String> = idle_keys(&kernels, now)
            .into_iter()
            .map(|k| k.session)
            .collect();
        assert_eq!(reaped, ["stale", "stale-child"]);
    }

    #[test]
    fn the_report_line_carries_the_cells_exit_code() {
        assert_eq!(exit_code("{\"cell\": \"1\", \"exit_code\": 3}\n"), Some(3));
        assert_eq!(exit_code("garbage"), None);
    }
}
