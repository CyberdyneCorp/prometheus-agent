//! The daemon's side of a confirmed `/update` (spec: distribution-and-
//! update, Update from chat): the same code path as `prometheus update`,
//! run as a detached child process of this binary (it holds the update
//! lock and outlives the daemon it replaces), its output in
//! `logs/update-from-chat.log`. A thread follows that log and reports the
//! stage lines and the exit to the chat's relay; the relay can also catch
//! up at once when this daemon stops for the handover. `/update check`
//! runs `update --check` and answers with what it printed.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_core::error::{Error, codes};
use pmt_gateway::messaging::{UpdateEvent, UpdateFeed, UpdateFromChat, UpdateRun};
use tokio::sync::mpsc;

/// The progress lines worth relaying start with a stage name.
const STAGES: [&str; 6] = [
    "checking",
    "downloading",
    "verifying",
    "self-test",
    "swapping",
    "restarting",
];

/// Where the detached updater writes its output.
fn log_path(home: &Path) -> PathBuf {
    home.join("logs").join("update-from-chat.log")
}

fn failed(what: &str, e: &std::io::Error) -> Error {
    Error::new(&codes::RPC_INTERNAL, format!("{what}: {e}"))
}

/// Runs `/update check`, or starts the update and follows it.
///
/// # Errors
///
/// When the updater cannot be started (or, for a check, fails to run).
pub(super) fn run(home: &Path, request: &UpdateFromChat) -> Result<UpdateRun, Error> {
    let exe = std::env::current_exe().map_err(|e| failed("cannot find this binary", &e))?;
    let mut command = Command::new(exe);
    command.arg("--home").arg(home).arg("update");
    if request.check {
        let out = command
            .arg("--check")
            .stdin(Stdio::null())
            .output()
            .map_err(|e| failed("the update check did not start", &e))?;
        let text =
            String::from_utf8_lossy(if out.status.success() || out.status.code() == Some(10) {
                &out.stdout
            } else {
                &out.stderr
            })
            .trim()
            .to_owned();
        return Ok(UpdateRun::Answer(text));
    }
    let log = log_path(home);
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).map_err(|e| failed("cannot create the log directory", &e))?;
    }
    let file =
        std::fs::File::create(&log).map_err(|e| failed("cannot create the update log", &e))?;
    let err = file
        .try_clone()
        .map_err(|e| failed("cannot create the update log", &e))?;
    if request.restart_busy {
        command.arg("--yes");
    }
    command.stdin(Stdio::null()).stdout(file).stderr(err);
    super::detach(&mut command);
    pmt_gateway::messaging::remember_update(home, request);
    let child = command.spawn().map_err(|e| {
        pmt_gateway::messaging::forget_update(home);
        failed("the updater did not start", &e)
    })?;
    let (tx, events) = mpsc::unbounded_channel();
    let reader = Arc::new(Mutex::new(Reader {
        log,
        tx,
        offset: 0,
        partial: String::new(),
        last_line: String::new(),
    }));
    let follower = Arc::clone(&reader);
    let home = home.to_owned();
    std::thread::spawn(move || follow(child, &follower, &home));
    Ok(UpdateRun::Running(UpdateFeed {
        events,
        catch_up: Box::new(move || reader.lock().expect("update reader poisoned").read()),
    }))
}

/// Reads the updater's log and reports its stage lines, from the
/// following thread or from the relay catching up; the lock keeps the
/// lines in order.
struct Reader {
    log: PathBuf,
    tx: mpsc::UnboundedSender<UpdateEvent>,
    offset: u64,
    partial: String,
    last_line: String,
}

impl Reader {
    /// Reports the stage lines written since the last read.
    fn read(&mut self) {
        for line in read_lines(&self.log, &mut self.offset, &mut self.partial) {
            if !line.trim().is_empty() {
                self.last_line.clone_from(&line);
            }
            if STAGES.iter().any(|stage| line.starts_with(stage)) {
                let _ = self.tx.send(UpdateEvent::Progress(line));
            }
        }
    }

    /// Reports the updater's exit, with the last line it printed.
    fn exited(&mut self, code: i32) {
        self.read();
        if !self.partial.trim().is_empty() {
            self.last_line = std::mem::take(&mut self.partial);
        }
        let _ = self.tx.send(UpdateEvent::Finished {
            code,
            last_line: std::mem::take(&mut self.last_line),
        });
    }
}

/// Reports the log's stage lines until the updater exits, then its exit.
fn follow(mut child: Child, reader: &Mutex<Reader>, home: &Path) {
    loop {
        let exited = child.try_wait().ok().flatten();
        let mut reader = reader.lock().expect("update reader poisoned");
        if let Some(status) = exited {
            // This daemon is still here: no new one will say the result.
            pmt_gateway::messaging::forget_update(home);
            reader.exited(status.code().unwrap_or(1));
            return;
        }
        reader.read();
        drop(reader);
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The complete lines appended to `log` since `offset`.
fn read_lines(log: &Path, offset: &mut u64, partial: &mut String) -> Vec<String> {
    let Ok(mut file) = std::fs::File::open(log) else {
        return Vec::new();
    };
    let mut text = String::new();
    if file.seek(SeekFrom::Start(*offset)).is_err() || file.read_to_string(&mut text).is_err() {
        return Vec::new();
    }
    *offset += text.len() as u64;
    partial.push_str(&text);
    let mut lines: Vec<String> = partial.split('\n').map(str::to_owned).collect();
    *partial = lines.pop().unwrap_or_default();
    lines
}
