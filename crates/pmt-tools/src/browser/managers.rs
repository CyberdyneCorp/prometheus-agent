//! Password-manager login sources of `browser_vault` (spec:
//! security-boundaries, Password manager login sources): the 1Password
//! `op` and Bitwarden `bw` CLIs offer their logins read-only next to the
//! Prometheus vault, when enabled and installed.
//!
//! A source is unlocked per session: the person types the master password
//! into a masked prompt, the CLI gets it through its non-interactive
//! channel (`op signin --raw` on stdin, `bw unlock --passwordenv` with a
//! child-only variable) and only the session token it prints is kept, in
//! memory, until 30 minutes pass without use. A session nobody can answer
//! (cron, webhook, API, single query) sees a locked source as
//! `unavailable_in_this_session` and is never prompted, except 1Password
//! with a service-account token. A manager login fills only on the exact
//! website URLs saved on the item (their origins, no subdomains).

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use pmt_core::secret::SecretString;
use serde_json::{Value, json};

/// How long an unlocked source stays unlocked without use.
pub const IDLE_RELOCK: Duration = Duration::from_mins(30);

/// How long one CLI call may take.
const TIMEOUT: Duration = Duration::from_secs(30);

/// A password manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    /// 1Password (`op`).
    OnePassword,
    /// Bitwarden (`bw`).
    Bitwarden,
}

impl Manager {
    /// Its configuration name (`onepassword`, `bitwarden`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::OnePassword => "onepassword",
            Self::Bitwarden => "bitwarden",
        }
    }

    /// The handle prefix of its items.
    const fn prefix(self) -> &'static str {
        match self {
            Self::OnePassword => "op_",
            Self::Bitwarden => "bw_",
        }
    }

    /// The source named `name`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        [Self::OnePassword, Self::Bitwarden]
            .into_iter()
            .find(|m| m.name() == name)
    }

    /// The source a handle belongs to, with the item id.
    #[must_use]
    pub fn of_handle(handle: &str) -> Option<(Self, &str)> {
        [Self::OnePassword, Self::Bitwarden]
            .into_iter()
            .find_map(|m| handle.strip_prefix(m.prefix()).map(|id| (m, id)))
            .filter(|(_, id)| !id.is_empty())
    }
}

/// One login of a manager: metadata the model may see, and the secret
/// values fetched only to fill.
#[derive(Debug, Clone)]
pub struct ManagerLogin {
    /// `op_<id>` / `bw_<id>`.
    pub handle: String,
    /// The item's title.
    pub label: String,
    /// The origins of its saved website URLs.
    pub origins: Vec<String>,
    /// The user name.
    pub identifier: String,
}

/// One enabled source of a session.
#[derive(Debug)]
pub struct Source {
    manager: Manager,
    program: PathBuf,
    /// The 1Password service-account token, when one is configured: the
    /// source then needs no unlock (and works headless).
    service_token: Option<SecretString>,
    unlocked: Mutex<Option<(SecretString, Instant)>>,
}

/// What a source is in this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Usable now.
    Unlocked,
    /// Needs the master password (`unlock`).
    Locked,
    /// Locked and nobody can be asked here.
    Unavailable,
}

impl State {
    /// The wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unlocked => "unlocked",
            Self::Locked => "locked",
            Self::Unavailable => "unavailable_in_this_session",
        }
    }
}

impl Source {
    /// The source `manager` run with `program`.
    #[must_use]
    pub fn new(manager: Manager, program: PathBuf, service_token: Option<SecretString>) -> Self {
        Self {
            manager,
            program,
            service_token,
            unlocked: Mutex::default(),
        }
    }

    /// Which manager.
    #[must_use]
    pub fn manager(&self) -> Manager {
        self.manager
    }

    /// The live session token (dropped after [`IDLE_RELOCK`] unused).
    fn token(&self) -> Option<SecretString> {
        let mut unlocked = self.unlocked.lock().unwrap_or_else(PoisonError::into_inner);
        match unlocked.as_mut() {
            Some((_, used)) if used.elapsed() >= IDLE_RELOCK => {
                *unlocked = None;
                None
            }
            Some((token, used)) => {
                *used = Instant::now();
                Some(token.clone())
            }
            None => None,
        }
    }

    /// The source's state for a session that can (`attended`) or cannot
    /// ask the person.
    #[must_use]
    pub fn state(&self, attended: bool) -> State {
        if self.service_token.is_some() || self.token().is_some() {
            State::Unlocked
        } else if attended {
            State::Locked
        } else {
            State::Unavailable
        }
    }

    /// Unlocks with `master` (from the masked prompt): the CLI turns it
    /// into a session token, which is all that is kept.
    ///
    /// # Errors
    ///
    /// A one-line reason (the master password is never in it).
    pub fn unlock(&self, master: &SecretString) -> Result<(), String> {
        let token = match self.manager {
            Manager::OnePassword => run(
                Command::new(&self.program).args(["signin", "--raw"]),
                Some(master),
            )?,
            Manager::Bitwarden => run(
                Command::new(&self.program)
                    .args(["unlock", "--passwordenv", "PMT_BW_MASTER", "--raw"])
                    .env("PMT_BW_MASTER", master.expose()),
                None,
            )?,
        };
        let token = String::from_utf8(token).map_err(|_| "the CLI printed no token".to_owned())?;
        let token = token.trim();
        if token.is_empty() {
            return Err("the CLI printed no session token".to_owned());
        }
        *self.unlocked.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((SecretString::new(token), Instant::now()));
        Ok(())
    }

    /// The CLI with the session's credentials in child-only variables.
    fn command(&self, args: &[&str]) -> Result<Command, String> {
        let mut command = Command::new(&self.program);
        command.args(args);
        for var in ["OP_SERVICE_ACCOUNT_TOKEN", "OP_SESSION", "BW_SESSION"] {
            command.env_remove(var);
        }
        match (self.manager, &self.service_token) {
            (Manager::OnePassword, Some(token)) => {
                command.env("OP_SERVICE_ACCOUNT_TOKEN", token.expose());
            }
            (Manager::OnePassword, None) => {
                let token = self.token().ok_or_else(locked)?;
                command.args(["--session", token.expose()]);
            }
            (Manager::Bitwarden, _) => {
                let token = self.token().ok_or_else(locked)?;
                command.env("BW_SESSION", token.expose());
            }
        }
        Ok(command)
    }

    /// The source's logins (metadata only).
    ///
    /// # Errors
    ///
    /// A locked source, a failing CLI.
    pub fn list(&self) -> Result<Vec<ManagerLogin>, String> {
        let (args, parse): (&[&str], ParseItem) = match self.manager {
            Manager::OnePassword => (
                &["item", "list", "--categories", "Login", "--format", "json"],
                op_meta,
            ),
            Manager::Bitwarden => (&["list", "items"], bw_meta),
        };
        let out = run(&mut self.command(args)?, None)?;
        let items: Vec<Value> = serde_json::from_slice(&out)
            .map_err(|_| "the CLI did not print a JSON list".to_owned())?;
        Ok(items.iter().filter_map(parse).collect())
    }

    /// The login `id`: its metadata and password.
    ///
    /// # Errors
    ///
    /// A locked source, a failing CLI, an item that is not a login.
    pub fn login(&self, id: &str) -> Result<(ManagerLogin, SecretString), String> {
        let out = match self.manager {
            Manager::OnePassword => run(
                &mut self.command(&["item", "get", id, "--format", "json", "--reveal"])?,
                None,
            )?,
            Manager::Bitwarden => run(&mut self.command(&["get", "item", id])?, None)?,
        };
        let item: Value =
            serde_json::from_slice(&out).map_err(|_| "the CLI did not print an item".to_owned())?;
        let (meta, password) = match self.manager {
            Manager::OnePassword => (op_meta(&item), op_field(&item, "password")),
            Manager::Bitwarden => (
                bw_meta(&item),
                item["login"]["password"].as_str().map(str::to_owned),
            ),
        };
        match (meta, password) {
            (Some(meta), Some(password)) => Ok((meta, SecretString::new(password))),
            _ => Err(format!("item {id} is not a login with a password")),
        }
    }
}

/// Reads one listed item of a CLI.
type ParseItem = fn(&Value) -> Option<ManagerLogin>;

fn locked() -> String {
    "the source is locked".to_owned()
}

/// The origins of saved website URLs (exact: no subdomains).
fn origins<'a>(urls: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut found: Vec<String> = urls.filter_map(pmt_vault::origin::of).collect();
    found.sort();
    found.dedup();
    found
}

fn op_field(item: &Value, id: &str) -> Option<String> {
    item["fields"].as_array()?.iter().find_map(|f| {
        (f["id"] == id || f["purpose"].as_str() == Some(&id.to_ascii_uppercase()))
            .then(|| f["value"].as_str().map(str::to_owned))?
    })
}

fn op_meta(item: &Value) -> Option<ManagerLogin> {
    let id = item["id"].as_str()?;
    let urls = item["urls"].as_array().map_or(&[][..], Vec::as_slice);
    Some(ManagerLogin {
        handle: format!("op_{id}"),
        label: item["title"].as_str().unwrap_or(id).to_owned(),
        origins: origins(urls.iter().filter_map(|u| u["href"].as_str())),
        identifier: op_field(item, "username")
            .or_else(|| item["additional_information"].as_str().map(str::to_owned))
            .unwrap_or_default(),
    })
}

fn bw_meta(item: &Value) -> Option<ManagerLogin> {
    let id = item["id"].as_str()?;
    let login = item.get("login").filter(|l| l.is_object())?;
    let uris = login["uris"].as_array().map_or(&[][..], Vec::as_slice);
    Some(ManagerLogin {
        handle: format!("bw_{id}"),
        label: item["name"].as_str().unwrap_or(id).to_owned(),
        origins: origins(uris.iter().filter_map(|u| u["uri"].as_str())),
        identifier: login["username"].as_str().unwrap_or_default().to_owned(),
    })
}

/// Runs `command` (stdin: `input` and EOF, else nothing); its stdout when
/// it exits 0 within [`TIMEOUT`]. Stderr is discarded: a CLI may echo
/// what it was given.
fn run(command: &mut Command, input: Option<&SecretString>) -> Result<Vec<u8>, String> {
    let mut child = command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("the CLI could not start ({})", e.kind()))?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        let _ = stdin.write_all(input.expose().as_bytes());
        let _ = stdin.write_all(b"\n");
    }
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("the CLI did not answer in time".to_owned());
            }
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("the CLI failed ({})", out.status));
    }
    Ok(out.stdout)
}

/// The JSON `list` shows for each source.
#[must_use]
pub fn source_entry(source: &Source, attended: bool) -> Value {
    json!({"source": source.manager().name(), "status": source.state(attended).as_str()})
}
