//! The Codex app-server runtime (spec: model-providers, Codex app-server
//! runtime).
//!
//! With `model.openai_runtime = "codex_app_server"`, a route on
//! `openai-codex`, `openai-api` or a custom provider the Codex config also
//! defines is handed to a long-lived `codex app-server` subprocess
//! (`model.codex_bin`, else `codex` found by the executable lookup). The
//! route's endpoint is the launch itself (`codex:///<program>`) and its
//! credential is Codex's own sign-in. The runtime is read when the route
//! is resolved, which a session does once from the configuration it
//! started with, so a change applies from the next session.
//!
//! One app-server process and Codex thread serve one session: the system
//! prompt goes once as the thread's developer instructions, and each model
//! call sends only the messages the thread has not seen as one Codex turn
//! ([`server`]). Codex runs its own tools; its messages and reasoning
//! stream back as text, and the commands, patches and tool calls it ran
//! come back as [`crate::StreamEvent::ExternalCall`] so the session's
//! transcript and trajectory record them ([`feed`]). Its approval requests
//! go to the session's approval gate through [`RuntimeApprovals`].

pub(crate) mod feed;
pub(crate) mod server;

pub use server::{CodexPlugin, close, plugins};

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use pmt_config::schema::OpenaiRuntime;
use pmt_core::error::Error;
use serde_json::Value;

use crate::classify::RetryPolicy;
use crate::registry::Provider;
use crate::resolve::{Credential, CredentialSource, Route, Target, Trace};
use crate::transport::Timeouts;
use crate::wire::WireSource;

/// The endpoint scheme of a Codex app-server route.
const SCHEME: &str = "codex:///";

/// The built-in providers whose turns the runtime takes.
pub const ELIGIBLE: &[&str] = &["openai-codex", "openai-api"];

/// Whether turns on `provider` go to the app-server under `runtime`:
/// the built-in OpenAI providers, and a custom provider the Codex config
/// at `codex_home` defines under `[model_providers.<id>]`.
#[must_use]
pub fn eligible(runtime: OpenaiRuntime, provider: &str, codex_home: Option<&Path>) -> bool {
    runtime == OpenaiRuntime::CodexAppServer
        && (ELIGIBLE.contains(&provider)
            || codex_home.is_some_and(|home| codex_providers(home).iter().any(|p| p == provider)))
}

/// The provider ids `[model_providers]` of `<codex_home>/config.toml`
/// defines (none when it is missing or unreadable).
#[must_use]
pub fn codex_providers(codex_home: &Path) -> Vec<String> {
    std::fs::read_to_string(codex_home.join("config.toml"))
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|table| table.get("model_providers")?.as_table().cloned())
        .map(|providers| providers.keys().cloned().collect())
        .unwrap_or_default()
}

/// Codex's home: `codex_home_var` (`CODEX_HOME`, Codex's own setting)
/// when set, else `~/.codex`.
#[must_use]
pub fn codex_home(
    codex_home_var: Option<std::ffi::OsString>,
    user_home: Option<&Path>,
) -> Option<PathBuf> {
    codex_home_var
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| user_home.map(|home| home.join(".codex")))
}

/// The route of `target` on `provider` run by the app-server at `program`.
pub(crate) fn route(
    provider: &Provider,
    target: &Target,
    program: &Path,
    trace: &mut Trace,
) -> Route {
    let base_url = format!(
        "{SCHEME}{}",
        program
            .to_string_lossy()
            .replace('\\', "/")
            .trim_start_matches('/')
    );
    trace.ok(
        "endpoint",
        format!("{base_url} (model.openai_runtime = codex_app_server)"),
    );
    let credential = Credential::new(
        CredentialSource::External {
            provider: provider.id().to_owned(),
        },
        None,
    );
    trace.ok("credential", credential.describe());
    // `default`: Codex picks its configured model.
    let model = target
        .model
        .clone()
        .or_else(|| provider.descriptor().default_model.clone())
        .unwrap_or_else(|| "default".to_owned());
    trace.ok("model", model.clone());
    let settings = provider.settings();
    Route {
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
        aggregator: None,
    }
}

/// The app-server program of a Codex route; `None` for any other route.
#[must_use]
pub fn program(route: &Route) -> Option<PathBuf> {
    let rest = route.base_url.strip_prefix(SCHEME)?;
    // `C:/x/codex.exe` on Windows, `/usr/bin/codex` elsewhere.
    Some(if cfg!(windows) && rest.get(1..2) == Some(":") {
        PathBuf::from(rest)
    } else {
        PathBuf::from(format!("/{rest}"))
    })
}

/// The executable the runtime would run, or why there is none:
/// `model.codex_bin` when set, else `codex` found on `path`, in the
/// user-local bin directories and the vendor's prefixes.
///
/// # Errors
///
/// `provider.external_cli_missing` naming `model.codex_bin` in its hint.
pub fn executable(
    codex_bin: Option<&str>,
    path: Option<std::ffi::OsString>,
    user_home: Option<&Path>,
) -> Result<PathBuf, Error> {
    crate::external::codex_executable(codex_bin, path, user_home).map_err(|err| {
        err.with_hint(
            "install the Codex CLI (npm install -g @openai/codex), or set model.codex_bin to its absolute path",
        )
    })
}

/// What Codex asks before it runs a command or changes files.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeAsk {
    /// The session the turn belongs to.
    pub session: Option<String>,
    /// Codex's item id (the call id the gate and transcript see).
    pub call_id: String,
    /// The Prometheus tool the action corresponds to (`terminal` for a
    /// command, `patch` for file changes).
    pub tool: &'static str,
    /// The action as that tool's arguments.
    pub arguments: Value,
}

/// Answers Codex's approval requests through the session's approval gate
/// (the same prompts, rules and `approval.request` the session's own tool
/// calls raise), and says where the session works.
pub trait RuntimeApprovals: Send + Sync {
    /// Whether the action may run.
    fn ask(&self, ask: RuntimeAsk) -> Pin<Box<dyn Future<Output = bool> + Send + '_>>;

    /// The workspace of `session`, where its Codex process and thread run
    /// (`None`: unknown, the process's own directory).
    fn workdir(&self, session: &str) -> Option<PathBuf>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eligibility_follows_the_runtime_and_the_codex_config() {
        let dir =
            tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")).unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "model = \"gpt-5\"\n[model_providers.corp]\nname = \"Corp\"\nbase_url = \"https://llm.corp/v1\"\n",
        )
        .unwrap();
        let home = Some(dir.path());
        let on = OpenaiRuntime::CodexAppServer;
        assert!(eligible(on, "openai-codex", None));
        assert!(eligible(on, "openai-api", None));
        assert!(eligible(on, "corp", home));
        assert!(!eligible(on, "anthropic", home));
        assert!(!eligible(OpenaiRuntime::Auto, "openai-codex", home));
    }
}
