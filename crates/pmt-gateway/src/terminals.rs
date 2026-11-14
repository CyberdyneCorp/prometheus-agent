//! Daemon-owned terminals (spec: desktop-app, Embedded terminals are
//! daemon-owned PTYs): interactive shells the daemon spawns as the user in
//! a session's workspace, so a local window, a remote one and a webapp tab
//! all reach the same shell. At most [`MAX`] exist per daemon; output goes
//! to the session's attached clients as `pty.output` and the last
//! [`REPLAY`] bytes are kept for a client that attaches later; a shell
//! nobody attached to the session for [`DETACHED_TTL`] is ended.
//!
//! One driver connection holds a terminal at a time (spec: web-dashboard,
//! Webapp file staging and terminals): the one that opened it or last
//! attached to it. A second window or tab attaching takes it over; the
//! one it took it from is told so and may type again only after it
//! attaches again.

use std::collections::{HashMap, VecDeque};
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use pmt_core::error::{Error, codes};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::hub::{ConnId, Hub};

/// The most PTYs a daemon keeps.
pub const MAX: usize = 16;
/// Output kept for a client that attaches later.
pub const REPLAY: usize = 1024 * 1024;
/// How long a shell outlives its session's last attached client.
pub const DETACHED_TTL: Duration = Duration::from_mins(15);

/// One shell.
struct Terminal {
    session_id: String,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn std::io::Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    replay: Arc<Mutex<VecDeque<u8>>>,
    /// When the session last had no attached client (`None`: it has one).
    alone_since: Option<Instant>,
    /// The driver connection that holds it.
    holder: Option<ConnId>,
}

/// Every terminal of the daemon, by id.
#[derive(Default)]
pub struct Terminals {
    open: Mutex<HashMap<String, Terminal>>,
    next: std::sync::atomic::AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn failed(what: &str, err: impl std::fmt::Display) -> Error {
    Error::new(&codes::TERMINAL_PTY, format!("{what}: {err}"))
}

fn unknown(id: &str) -> Error {
    Error::new(
        &codes::TERMINAL_NOT_FOUND,
        format!("no terminal {id} in this session"),
    )
}

/// The user's interactive shell.
fn shell() -> CommandBuilder {
    if cfg!(windows) {
        CommandBuilder::new(std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_owned()))
    } else {
        let mut cmd =
            CommandBuilder::new(std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned()));
        cmd.arg("-i");
        cmd
    }
}

impl Terminals {
    /// Opens a shell for `session_id` in `cwd`; its output is published on
    /// the session through `hub`.
    ///
    /// # Errors
    ///
    /// `terminal.limit` when [`MAX`] are open, `terminal.pty` when the
    /// PTY or the shell cannot start.
    pub fn open(
        self: &Arc<Self>,
        hub: &Arc<Hub>,
        session_id: &str,
        cwd: &Path,
        (cols, rows): (u16, u16),
    ) -> Result<String, Error> {
        let mut open = lock(&self.open);
        if open.len() >= MAX {
            return Err(Error::new(
                &codes::TERMINAL_LIMIT,
                format!("terminal limit reached ({MAX}); close a terminal first"),
            ));
        }
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| failed("cannot open a terminal", e))?;
        let mut cmd = shell();
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| failed("cannot start the shell", e))?;
        drop(pair.slave);
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| failed("terminal output", e))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| failed("terminal input", e))?;
        let id = format!(
            "pty_{}",
            self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
        );
        let replay = Arc::new(Mutex::new(VecDeque::new()));
        self.pump(
            reader,
            Arc::clone(hub),
            (session_id.to_owned(), id.clone()),
            Arc::clone(&replay),
        );
        open.insert(
            id.clone(),
            Terminal {
                session_id: session_id.to_owned(),
                master: pair.master,
                writer,
                child,
                replay,
                alone_since: None,
                holder: None,
            },
        );
        Ok(id)
    }

    /// Reads the shell's output on a thread of its own until it ends.
    fn pump(
        self: &Arc<Self>,
        mut reader: Box<dyn std::io::Read + Send>,
        hub: Arc<Hub>,
        (session_id, id): (String, String),
        replay: Arc<Mutex<VecDeque<u8>>>,
    ) {
        let terminals = Arc::downgrade(self);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                {
                    let mut kept = lock(&replay);
                    kept.extend(&buf[..n]);
                    let over = kept.len().saturating_sub(REPLAY);
                    kept.drain(..over);
                }
                let data = base64::engine::general_purpose::STANDARD.encode(&buf[..n]);
                hub.publish(
                    &session_id,
                    "pty.output",
                    serde_json::json!({"pty_id": id, "data": data}),
                );
            }
            if let Some(terminals) = terminals.upgrade() {
                lock(&terminals.open).remove(&id);
            }
            hub.publish(&session_id, "pty.exited", serde_json::json!({"pty_id": id}));
        });
    }

    fn with<T>(
        &self,
        session_id: &str,
        id: &str,
        f: impl FnOnce(&mut Terminal) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let mut open = lock(&self.open);
        let terminal = open
            .get_mut(id)
            .filter(|t| t.session_id == session_id)
            .ok_or_else(|| unknown(id))?;
        f(terminal)
    }

    /// The output kept for a client attaching now (base64).
    ///
    /// # Errors
    ///
    /// `terminal.not_found` (another session's or an ended terminal).
    pub fn replay(&self, session_id: &str, id: &str) -> Result<String, Error> {
        self.with(session_id, id, |t| {
            let kept = lock(&t.replay);
            let (a, b) = kept.as_slices();
            Ok(base64::engine::general_purpose::STANDARD.encode([a, b].concat()))
        })
    }

    /// Makes `conn` the terminal's holder; the connection that held it
    /// before, when another one.
    ///
    /// # Errors
    ///
    /// `terminal.not_found` (another session's or an ended terminal).
    pub fn hold(&self, session_id: &str, id: &str, conn: ConnId) -> Result<Option<ConnId>, Error> {
        self.with(session_id, id, |t| {
            Ok(t.holder.replace(conn).filter(|before| *before != conn))
        })
    }

    /// The connection that holds the terminal.
    ///
    /// # Errors
    ///
    /// `terminal.not_found` (another session's or an ended terminal).
    pub fn holder(&self, session_id: &str, id: &str) -> Result<Option<ConnId>, Error> {
        self.with(session_id, id, |t| Ok(t.holder))
    }

    /// Types `data` into the shell.
    ///
    /// # Errors
    ///
    /// `terminal.not_found` (another session's or an ended terminal).
    pub fn input(&self, session_id: &str, id: &str, data: &[u8]) -> Result<(), Error> {
        self.with(session_id, id, |t| {
            t.writer
                .write_all(data)
                .and_then(|()| t.writer.flush())
                .map_err(|e| failed("terminal input", e))
        })
    }

    /// Resizes the terminal.
    ///
    /// # Errors
    ///
    /// `terminal.not_found`, `terminal.pty`.
    pub fn resize(
        &self,
        session_id: &str,
        id: &str,
        (cols, rows): (u16, u16),
    ) -> Result<(), Error> {
        self.with(session_id, id, |t| {
            t.master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|e| failed("terminal resize", e))
        })
    }

    /// Ends the shell.
    ///
    /// # Errors
    ///
    /// `terminal.not_found` (another session's or an ended terminal).
    pub fn close(&self, session_id: &str, id: &str) -> Result<(), Error> {
        let mut terminal = {
            let mut open = lock(&self.open);
            if open.get(id).is_none_or(|t| t.session_id != session_id) {
                return Err(unknown(id));
            }
            open.remove(id).ok_or_else(|| unknown(id))?
        };
        let _ = terminal.child.kill();
        Ok(())
    }

    /// The terminals of `session_id`.
    pub fn of_session(&self, session_id: &str) -> Vec<String> {
        lock(&self.open)
            .iter()
            .filter(|(_, t)| t.session_id == session_id)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Ends the shells whose session had no attached client for
    /// [`DETACHED_TTL`] (`attended` says whether a session has one now).
    pub fn reap(&self, attended: impl Fn(&str) -> bool, now: Instant) {
        let mut open = lock(&self.open);
        let mut ended = Vec::new();
        for (id, t) in open.iter_mut() {
            if attended(&t.session_id) {
                t.alone_since = None;
            } else if now.duration_since(*t.alone_since.get_or_insert(now)) >= DETACHED_TTL {
                ended.push(id.clone());
            }
        }
        for id in ended {
            if let Some(mut t) = open.remove(&id) {
                let _ = t.child.kill();
            }
        }
    }

    /// Ends every shell (the daemon stops).
    pub fn close_all(&self) {
        for (_, mut t) in lock(&self.open).drain() {
            let _ = t.child.kill();
        }
    }
}
