//! `external_process` providers (spec: model-providers, Plugin OAuth and
//! external-process providers): a provider that drives an installed agent
//! CLI (Claude Code, for example) over the Agent Client Protocol with that
//! CLI's own sign-in.
//!
//! The executable is `providers.<id>.command` (a plugin may only suggest
//! one: the host launches what the user configured), found by the
//! executable lookup: `PATH`, then the user-local bin
//! directories agent CLIs install themselves into. A missing one fails the
//! route with `provider.external_cli_missing` naming the command. The
//! route's endpoint is the launch itself (`acp:///<program>?arg=...`), so
//! two routes differing only in their CLI stay distinct.
//!
//! Each model call spawns the CLI in the process working directory, opens
//! one ACP session, sends the conversation as one prompt and streams the
//! agent's message and thought chunks back as the normalised event
//! stream. The CLI runs its own tools under its own permissions; every
//! permission request it sends is answered `cancelled`, and the file
//! system and terminal client methods are not offered.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use pmt_core::error::{Error, codes};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};

use crate::classify::{ErrorClass, ProviderError, RetryPolicy};
use crate::event::{Normalizer, StopReason};
use crate::registry::Provider;
use crate::request::{Part, Request, Role};
use crate::resolve::{Credential, CredentialSource, Route, Target, Trace};
use crate::transport::{Limits, Timeouts};
use crate::wire::WireSource;

/// The ACP version this client speaks.
const PROTOCOL_VERSION: u64 = 1;
/// The endpoint scheme of an external route.
const SCHEME: &str = "acp://";

/// Directories searched after `PATH`, relative to the user's home: where
/// agent CLIs install themselves for one user.
const USER_BINS: &[&str] = &[
    ".local/bin",
    "bin",
    ".npm-global/bin",
    ".claude/local",
    ".bun/bin",
    ".cargo/bin",
];

/// The install prefixes each vendor documents, searched last: user-home
/// relative ones (`~/...`) and system ones. A gateway service, cron run
/// or desktop launch with a bare `PATH` still finds the CLI an
/// interactive shell would.
const VENDOR_PREFIXES: &[(&str, &[&str])] = &[
    ("claude", &["~/.claude/local", "~/.claude/bin"]),
    ("codex", &["~/.codex/bin"]),
    ("gemini", &["~/.gemini/bin"]),
    ("opencode", &["~/.opencode/bin"]),
    ("qwen", &["~/.qwen/bin"]),
];

/// System prefixes package managers install agent CLIs into.
const SYSTEM_PREFIXES: &[&str] = if cfg!(windows) {
    &[]
} else {
    &["/opt/homebrew/bin", "/usr/local/bin", "/snap/bin"]
};

/// The executable `command` names: itself when it is a path, else the
/// first match on `path`, in the user-local bin directories of `home`,
/// then in the vendor's documented install prefixes.
#[must_use]
pub fn locate(command: &str, path: Option<OsString>, home: Option<&Path>) -> Option<PathBuf> {
    let direct = Path::new(command);
    if direct.components().count() > 1 {
        return direct.is_file().then(|| direct.to_path_buf());
    }
    let mut dirs: Vec<PathBuf> = path
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(home) = home {
        dirs.extend(USER_BINS.iter().map(|dir| home.join(dir)));
    }
    dirs.extend(vendor_dirs(command, home));
    let names: Vec<String> = if cfg!(windows) {
        [".exe", ".cmd", ".bat", ""]
            .iter()
            .map(|ext| format!("{command}{ext}"))
            .collect()
    } else {
        vec![command.to_owned()]
    };
    dirs.iter()
        .filter(|dir| !dir.as_os_str().is_empty())
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

/// `command`'s vendor prefixes, then the system prefixes.
fn vendor_dirs(command: &str, home: Option<&Path>) -> Vec<PathBuf> {
    let own = VENDOR_PREFIXES
        .iter()
        .find(|(name, _)| *name == command)
        .map_or(&[][..], |(_, dirs)| *dirs);
    own.iter()
        .filter_map(|dir| match dir.strip_prefix("~/") {
            Some(rest) => home.map(|h| h.join(rest)),
            None => Some(PathBuf::from(dir)),
        })
        .chain(SYSTEM_PREFIXES.iter().map(PathBuf::from))
        .collect()
}

/// The executable at `command_path`: one absolute path to a file, never
/// split or parsed as a shell command.
///
/// # Errors
///
/// `config.invalid` for a relative path, `provider.external_cli_missing`
/// when no file is there.
pub fn pinned(key: &str, provider: &str, command_path: &str) -> Result<PathBuf, Error> {
    let path = Path::new(command_path);
    if !path.is_absolute() {
        return Err(Error::new(
            &codes::CONFIG_INVALID,
            format!("{key} '{command_path}' must be an absolute path to the executable"),
        ));
    }
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    Err(missing(provider, command_path))
}

/// The Codex CLI the Codex app-server runtime runs: `model.codex_bin`,
/// else `codex` found by [`locate`].
///
/// # Errors
///
/// As [`pinned`]; `provider.external_cli_missing` when `codex` is not
/// found.
pub fn codex_executable(
    codex_bin: Option<&str>,
    path: Option<OsString>,
    home: Option<&Path>,
) -> Result<PathBuf, Error> {
    match codex_bin {
        Some(bin) => pinned("model.codex_bin", "openai-codex", bin),
        None => locate("codex", path, home).ok_or_else(|| missing("openai-codex", "codex")),
    }
}

/// `provider.external_cli_missing` naming `command`.
#[must_use]
pub fn missing(provider: &str, command: &str) -> Error {
    Error::new(
        &codes::PROVIDER_EXTERNAL_CLI_MISSING,
        format!(
            "provider '{provider}' drives the agent CLI '{command}', which is not installed (not on PATH, in the user-local bin directories or the vendor's install prefixes)"
        ),
    )
    .with_hint(format!(
        "install {command}, or set providers.{provider}.command_path to its absolute path"
    ))
}

/// The user's home directory, for the user-local bin directories.
fn user_home() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The endpoint of an `external_process` provider: the located
/// `providers.<id>.command` and its `args`. Only the user's configuration
/// names what is launched; a plugin's suggested command is shown in the
/// hint.
///
/// # Errors
///
/// `provider.missing_base_url` when no command is configured,
/// `provider.external_cli_missing` when it is not found.
pub fn endpoint(provider: &Provider) -> Result<String, Error> {
    let id = provider.id();
    let settings = provider.settings();
    if let Some(command_path) = settings.command_path.as_deref() {
        let key = format!("providers.{id}.command_path");
        return Ok(launch_url(&pinned(&key, id, command_path)?, &settings.args));
    }
    let Some(command) = settings.command.as_deref() else {
        let suggested = provider
            .descriptor()
            .command
            .as_deref()
            .unwrap_or("<agent CLI>");
        return Err(Error::new(
            &codes::PROVIDER_MISSING_BASE_URL,
            format!(
                "provider '{id}' drives an agent CLI but names none: set providers.{id}.command"
            ),
        )
        .with_hint(format!(
            "prometheus config set providers.{id}.command {suggested}"
        )));
    };
    let program = locate(command, std::env::var_os("PATH"), user_home().as_deref())
        .ok_or_else(|| missing(id, command))?;
    Ok(launch_url(&program, &settings.args))
}

/// The route of `target` on `external_process` provider `provider`: its
/// located CLI as the endpoint, the CLI's own sign-in as the credential,
/// and the requested model (else the descriptor's, else `default`: the
/// CLI picks its own).
///
/// # Errors
///
/// As [`endpoint`].
pub(crate) fn route(
    provider: &Provider,
    target: &Target,
    trace: &mut Trace,
) -> Result<Route, Error> {
    let base_url = trace.check("endpoint", endpoint(provider))?;
    trace.ok("endpoint", base_url.clone());
    let credential = Credential::new(
        CredentialSource::External {
            provider: provider.id().to_owned(),
        },
        None,
    );
    trace.ok("credential", credential.describe());
    let model = target
        .model
        .clone()
        .or_else(|| provider.descriptor().default_model.clone())
        .unwrap_or_else(|| "default".to_owned());
    trace.ok("model", model.clone());
    let settings = provider.settings();
    Ok(Route {
        provider: provider.id().to_owned(),
        model,
        wire_format: provider.descriptor().wire_format,
        wire_source: WireSource::Descriptor,
        base_url,
        credential,
        supports_prompt_cache: false,
        supports_server_compaction: false,
        local: false,
        retry: RetryPolicy::new(settings.max_retries),
        timeouts: Timeouts {
            request: std::time::Duration::from_secs(settings.request_timeout_secs),
            idle: std::time::Duration::from_secs(settings.stream_idle_timeout_secs),
        },
        affinity_header: None,
        body_extra: None,
        fast_tier: None,
        bundled: None,
        key_header: None,
        region: None,
    })
}

/// `acp:///<program>?arg=<a>&arg=<b>`.
fn launch_url(program: &Path, args: &[String]) -> String {
    let mut url = url::Url::parse("acp:///").expect("static URL");
    url.set_path(&program.to_string_lossy().replace('\\', "/"));
    if !args.is_empty() {
        let mut query = url.query_pairs_mut();
        for arg in args {
            query.append_pair("arg", arg);
        }
    }
    url.to_string()
}

/// The program and arguments of an external route; `None` for any other.
#[must_use]
pub fn launch(route: &Route) -> Option<(PathBuf, Vec<String>)> {
    parse_launch(&route.base_url)
}

fn parse_launch(endpoint: &str) -> Option<(PathBuf, Vec<String>)> {
    if !endpoint.starts_with(SCHEME) {
        return None;
    }
    let url = url::Url::parse(endpoint).ok()?;
    // The URL path is percent-encoded (a space in a directory name).
    let path = percent_encoding::percent_decode_str(url.path())
        .decode_utf8()
        .ok()?;
    let path = path.as_ref();
    // `/C:/x/claude.exe` on Windows.
    let program = match path.get(2..3) {
        Some(":") if cfg!(windows) => PathBuf::from(&path[1..]),
        _ => PathBuf::from(path),
    };
    let args = url
        .query_pairs()
        .filter(|(k, _)| k == "arg")
        .map(|(_, v)| v.into_owned())
        .collect();
    Some((program, args))
}

/// The conversation as one prompt text: the system blocks, then each
/// message with its role. Tool calls and results of earlier turns are
/// written as text; the CLI's own tools do the work now.
fn prompt_text(request: &Request) -> String {
    let mut text = String::new();
    for block in &request.system {
        text.push_str(&block.text);
        text.push_str("\n\n");
    }
    for message in &request.messages {
        let role = match message.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::Tool => "Tool",
        };
        let body = message
            .parts
            .iter()
            .filter_map(part_text)
            .collect::<Vec<_>>()
            .join("\n");
        if !body.is_empty() {
            let _ = write!(text, "{role}: {body}\n\n");
        }
    }
    text
}

fn part_text(part: &Part) -> Option<String> {
    match part {
        Part::Text { text } => Some(text.clone()),
        Part::Image { .. } => None,
        Part::ToolCall {
            name, arguments, ..
        } => Some(format!("[called {name} {arguments}]")),
        Part::ToolResult { content, .. } => Some(format!("[result] {content}")),
    }
}

fn failed(why: &str) -> ProviderError {
    ProviderError::new(
        ErrorClass::ServerError,
        format!("external agent CLI: {why}"),
    )
}

/// A running ACP session of one model call.
#[derive(Debug)]
pub(crate) struct AcpFeed {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    prompt_id: u64,
    done: bool,
}

impl AcpFeed {
    /// Spawns `program args`, initialises ACP, opens a session and sends
    /// `request` as its prompt.
    pub(crate) async fn start(
        (program, args): (PathBuf, Vec<String>),
        request: &Request,
        limits: Limits,
    ) -> Result<Self, ProviderError> {
        let cwd = std::env::current_dir().map_err(|e| failed(&e.to_string()))?;
        let mut child = tokio::process::Command::new(&program)
            .args(&args)
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| failed(&format!("cannot start {}: {e}", program.display())))?;
        let stdin = child.stdin.take().ok_or_else(|| failed("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| failed("no stdout"))?;
        let mut feed = Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            prompt_id: 3,
            done: false,
        };
        let capabilities =
            json!({"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false});
        let init = json!({"protocolVersion": PROTOCOL_VERSION, "clientCapabilities": capabilities});
        limits.within(feed.call(1, "initialize", init)).await??;
        let new = json!({"cwd": cwd.to_string_lossy(), "mcpServers": []});
        let session = limits.within(feed.call(2, "session/new", new)).await??;
        let id = session["sessionId"]
            .as_str()
            .ok_or_else(|| failed("session/new returned no sessionId"))?
            .to_owned();
        let prompt = json!([{"type": "text", "text": prompt_text(request)}]);
        feed.send(&json!({
            "jsonrpc": "2.0",
            "id": feed.prompt_id,
            "method": "session/prompt",
            "params": {"sessionId": id, "prompt": prompt},
        }))
        .await?;
        Ok(feed)
    }

    async fn send(&mut self, frame: &Value) -> Result<(), ProviderError> {
        let mut line = frame.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| failed(&e.to_string()))
    }

    async fn next_frame(&mut self) -> Result<Value, ProviderError> {
        loop {
            let line = self
                .lines
                .next_line()
                .await
                .map_err(|e| failed(&e.to_string()))?
                .ok_or_else(|| failed("the CLI exited before answering"))?;
            if !line.trim().is_empty() {
                return serde_json::from_str(&line).map_err(|_| failed("a frame is not JSON"));
            }
        }
    }

    /// Sends request `id` and waits for its result, answering the agent's
    /// own requests on the way.
    async fn call(&mut self, id: u64, method: &str, params: Value) -> Result<Value, ProviderError> {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        loop {
            let frame = self.next_frame().await?;
            if is_answer(&frame, id) {
                return result_of(&frame, method);
            }
            self.answer(&frame).await?;
        }
    }

    /// Answers a request the agent sent: permissions are cancelled, other
    /// client methods are not offered.
    async fn answer(&mut self, frame: &Value) -> Result<(), ProviderError> {
        let (Some(method), Some(id)) = (frame["method"].as_str(), frame.get("id")) else {
            return Ok(());
        };
        let reply = if method == "session/request_permission" {
            json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": {"outcome": "cancelled"}}})
        } else {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("{method} is not offered")}})
        };
        self.send(&reply).await
    }

    /// Streams the next update into `out`, or ends the stream when the
    /// prompt's answer arrives.
    pub(crate) async fn step(
        &mut self,
        out: &mut Normalizer,
        limits: Limits,
    ) -> Result<(), ProviderError> {
        if self.done {
            out.finish(None);
            return Ok(());
        }
        let frame = limits.within(self.next_frame()).await??;
        if is_answer(&frame, self.prompt_id) {
            let result = result_of(&frame, "session/prompt")?;
            self.done = true;
            let _ = self.child.start_kill();
            out.finish(Some(stop_reason(&result)));
            return Ok(());
        }
        if frame["method"].as_str() == Some("session/update") {
            let update = &frame["params"]["update"];
            let text = update["content"]["text"].as_str().unwrap_or_default();
            match update["sessionUpdate"].as_str() {
                Some("agent_message_chunk") => out.text(text),
                Some("agent_thought_chunk") => out.reasoning(text),
                _ => {}
            }
            return Ok(());
        }
        self.answer(&frame).await
    }
}

fn is_answer(frame: &Value, id: u64) -> bool {
    frame["id"].as_u64() == Some(id) && frame.get("method").is_none()
}

fn stop_reason(result: &Value) -> StopReason {
    match result["stopReason"].as_str() {
        Some("max_tokens") => StopReason::MaxTokens,
        Some("refusal") => StopReason::ContentFilter,
        _ => StopReason::EndTurn,
    }
}

fn result_of(frame: &Value, method: &str) -> Result<Value, ProviderError> {
    if let Some(error) = frame.get("error") {
        return Err(failed(&format!(
            "{method} failed: {}",
            error["message"].as_str().unwrap_or("error")
        )));
    }
    Ok(frame["result"].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_is_found_on_path_or_in_the_user_bins() {
        let dir =
            tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")).unwrap();
        let bins = dir.path().join(".local/bin");
        std::fs::create_dir_all(&bins).unwrap();
        let name = if cfg!(windows) {
            "agentcli.exe"
        } else {
            "agentcli"
        };
        std::fs::write(bins.join(name), "").unwrap();
        let nowhere = Some(OsString::from(dir.path().join("nowhere")));
        assert_eq!(
            locate("agentcli", nowhere.clone(), Some(dir.path())),
            Some(bins.join(name))
        );
        assert_eq!(locate("agentcli", nowhere.clone(), None), None);
        assert_eq!(locate("claude", nowhere, Some(dir.path())), None);
        let on_path = Some(OsString::from(&bins));
        assert_eq!(locate("agentcli", on_path, None), Some(bins.join(name)));
    }

    #[test]
    fn the_launch_round_trips_through_the_endpoint() {
        let program = if cfg!(windows) {
            PathBuf::from("C:/tools/claude.exe")
        } else {
            PathBuf::from("/opt/tools/claude")
        };
        let args = vec!["--acp".to_owned(), "a b&c".to_owned()];
        let endpoint = launch_url(&program, &args);
        assert!(endpoint.starts_with("acp:///"), "{endpoint}");
        assert_eq!(parse_launch(&endpoint), Some((program, args)));
        assert_eq!(parse_launch("https://api.example.com/v1"), None);
    }
}
