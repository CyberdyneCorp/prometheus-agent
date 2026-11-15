//! The mailbox between a sandboxed worker lane and its broker (spec:
//! kanban, Worker lanes). The lane's sandbox may write only this directory
//! (`<home>/kanban/lanes/<run>/`), never the board: its `prometheus kanban
//! <verb> --ticket -` writes a request here, the broker (outside the
//! sandbox) checks the ticket and applies it, and writes the answer back.
//!
//! Files: `<id>.req` (a [`Request`], written as `<id>.req.tmp` then
//! renamed), `<id>-<n>.bin` (the bytes of `kanban attach`'s n-th file,
//! copied by the lane, so the broker never opens a path the lane names)
//! and `<id>.res` (a [`Response`]). `<id>` is a ULID.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pmt_core::error::{ERROR_CODES, Error, codes};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The largest request read (the arguments and the ticket).
const MAX_REQUEST: u64 = 1024 * 1024;

/// How long a lane waits for its broker's answer.
const ANSWER_TIMEOUT: Duration = Duration::from_mins(5);

/// How often the lane looks for its answer.
const POLL: Duration = Duration::from_millis(50);

/// A lane's request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// The `prometheus` arguments after the program name.
    pub argv: Vec<String>,
    /// The run ticket line.
    pub ticket: String,
    /// The staged copies of `kanban attach`'s files, in order.
    pub staged: Vec<String>,
}

/// The broker's answer: the command's JSON result or its error.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Response {
    /// The result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<Value>,
    /// The error code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// The error message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl Response {
    fn of(result: Result<Value, Error>) -> Self {
        match result {
            Ok(value) => Self {
                ok: Some(value),
                ..Self::default()
            },
            Err(err) => Self {
                ok: None,
                code: Some(err.code().code.to_owned()),
                message: Some(err.message().to_owned()),
            },
        }
    }

    fn into_result(self) -> Result<Value, Error> {
        if let Some(value) = self.ok {
            return Ok(value);
        }
        let named = self.code.as_deref().unwrap_or_default();
        let code = ERROR_CODES
            .iter()
            .copied()
            .find(|c| c.code == named)
            .unwrap_or(&codes::KANBAN_NOT_PERMITTED);
        Err(Error::new(code, self.message.unwrap_or_default()))
    }
}

fn failed(message: impl Into<String>) -> Error {
    Error::new(&codes::KANBAN_NOT_PERMITTED, message)
}

/// The mailbox directory of run `run` in the home at `home`.
#[must_use]
pub fn dir(home: &Path, run: &str) -> PathBuf {
    home.join("kanban/lanes").join(run)
}

/// Whether `name` is a mailbox id (a ULID: 26 alphanumerics).
fn is_id(name: &str) -> bool {
    name.len() == 26 && name.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Whether `name` is a staged file of request `id`.
fn is_staged(id: &str, name: &str) -> bool {
    name.strip_prefix(id)
        .and_then(|rest| rest.strip_prefix('-'))
        .and_then(|rest| rest.strip_suffix(".bin"))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| Error::io(&codes::HOME_IO, path, &e))
}

/// The lane side: stages `files`, sends the request and waits for the
/// answer.
///
/// # Errors
///
/// The broker's error; `kanban.not_permitted` when it does not answer;
/// `home.io`.
pub fn send(
    mailbox: &Path,
    argv: Vec<String>,
    ticket: String,
    files: &[PathBuf],
) -> Result<Value, Error> {
    let id = ulid::Ulid::generate().to_string();
    let mut staged = Vec::new();
    for (n, file) in files.iter().enumerate() {
        let name = format!("{id}-{n}.bin");
        std::fs::copy(file, mailbox.join(&name))
            .map_err(|e| Error::io(&codes::HOME_IO, file, &e))?;
        staged.push(name);
    }
    let request = Request {
        argv,
        ticket,
        staged,
    };
    let body = serde_json::to_vec(&request).map_err(|e| failed(e.to_string()))?;
    write_atomic(&mailbox.join(format!("{id}.req")), &body)?;
    let answer = mailbox.join(format!("{id}.res"));
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    while !answer.exists() {
        if Instant::now() > deadline {
            return Err(failed("the lane's broker did not answer"));
        }
        std::thread::sleep(POLL);
    }
    let text = std::fs::read(&answer).map_err(|e| Error::io(&codes::HOME_IO, &answer, &e))?;
    let _ = std::fs::remove_file(&answer);
    serde_json::from_slice::<Response>(&text)
        .map_err(|e| failed(format!("unreadable broker answer: {e}")))?
        .into_result()
}

/// The broker side: answers every pending request with `handle` (given
/// the request and the mailbox), then removes it and its staged files.
pub fn serve(mailbox: &Path, handle: &mut dyn FnMut(&Request, &Path) -> Result<Value, Error>) {
    let Ok(entries) = std::fs::read_dir(mailbox) else {
        return;
    };
    let mut ids: Vec<String> = entries
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter_map(|name| name.strip_suffix(".req").map(str::to_owned))
        .filter(|id| is_id(id))
        .collect();
    ids.sort();
    for id in ids {
        let path = mailbox.join(format!("{id}.req"));
        let result = read_request(&path).and_then(|request| {
            if request.staged.iter().any(|name| !is_staged(&id, name)) {
                return Err(failed("a staged file is not this request's"));
            }
            handle(&request, mailbox)
        });
        let body = serde_json::to_vec(&Response::of(result)).unwrap_or_default();
        let _ = std::fs::remove_file(&path);
        remove_staged(mailbox, &id);
        if let Err(err) = write_atomic(&mailbox.join(format!("{id}.res")), &body) {
            tracing::warn!("kanban lane mailbox: {}", err.message());
        }
    }
}

fn read_request(path: &Path) -> Result<Request, Error> {
    let bytes = pmt_kanban::attach::read_limited(path, MAX_REQUEST)
        .map_err(|_| failed("the lane's request is too large or unreadable"))?;
    serde_json::from_slice(&bytes).map_err(|e| failed(format!("invalid lane request: {e}")))
}

fn remove_staged(mailbox: &Path, id: &str) {
    let Ok(entries) = std::fs::read_dir(mailbox) else {
        return;
    };
    for name in entries.filter_map(|e| e.ok()?.file_name().into_string().ok()) {
        if is_staged(id, &name) {
            let _ = std::fs::remove_file(mailbox.join(name));
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A request goes through the mailbox and its answer, success or
    /// error, comes back with the error's own code; staged files are
    /// handed over and then removed.
    #[test]
    fn a_request_and_its_answer_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mailbox = dir.path().to_path_buf();
        let file = dir.path().join("report.txt");
        std::fs::write(&file, "numbers").unwrap();
        let broker = {
            let mailbox = mailbox.clone();
            std::thread::spawn(move || {
                let mut seen = 0;
                while seen < 2 {
                    serve(&mailbox, &mut |request, at| {
                        seen += 1;
                        if request.argv[0] == "bad" {
                            return Err(Error::new(&codes::KANBAN_TICKET_REFUSED, "no"));
                        }
                        let staged = std::fs::read_to_string(at.join(&request.staged[0])).unwrap();
                        Ok(json!({"ticket": request.ticket, "staged": staged}))
                    });
                    std::thread::sleep(Duration::from_millis(10));
                }
            })
        };
        let ok = send(
            &mailbox,
            vec!["good".into()],
            "t".into(),
            std::slice::from_ref(&file),
        )
        .unwrap();
        assert_eq!(ok, json!({"ticket": "t", "staged": "numbers"}));
        let err = send(&mailbox, vec!["bad".into()], "t".into(), &[]).unwrap_err();
        assert_eq!(err.code().code, "kanban.ticket_refused");
        broker.join().unwrap();
        let left: Vec<_> = std::fs::read_dir(&mailbox)
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n != "report.txt")
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn staged_names_belong_to_their_request() {
        let id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        assert!(is_staged(id, &format!("{id}-0.bin")));
        assert!(!is_staged(id, "../board.db"));
        assert!(!is_staged(id, &format!("{id}-x.bin")));
        assert!(!is_staged(id, "01ARZ3NDEKTSV4RRFFQ69G5FAW-0.bin"));
    }
}
