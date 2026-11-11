//! OpenClaw (`~/.openclaw`, legacy `~/.clawdbot` and `~/.moltbot`; spec:
//! hermes-migration, OpenClaw mapping).
//!
//! `--preset user-data` (the default) imports what the user made: the
//! workspace identity, memory, `AGENTS.md` (only into
//! `--workspace-target`), skills, MCP servers and the time zone.
//! `--preset full` adds the infrastructure: models and providers,
//! reasoning effort, turn limits, compaction, exec timeout and the Docker
//! sandbox. Provider API keys are secrets: stored only with
//! `--migrate-secrets`, else reported with that flag. Messaging channels
//! and session lifetime settings have no mapping and are reported.

use std::path::PathBuf;

use pmt_store::{MemoryKind, MemoryTarget};
use serde_json::Value;

use super::build::Build;
use super::mcp::Secrets;
use crate::component::Component;
use crate::report::{Disposition, Entry};

pub use super::steps::SkillConflict;

/// `--preset`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Preset {
    /// Identity, memory, skills, MCP servers, time zone.
    #[default]
    UserData,
    /// Also models, providers, sandbox and other infrastructure.
    Full,
}

/// OpenClaw's options.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    /// `--preset`.
    pub preset: Preset,
    /// `--migrate-secrets`.
    pub migrate_secrets: bool,
    /// `--workspace-target`.
    pub workspace_target: Option<PathBuf>,
    /// `--skill-conflict`.
    pub skill_conflict: SkillConflict,
}

/// The config file names, newest first.
const CONFIG_FILES: &[&str] = &["openclaw.json", "clawdbot.json", "moltbot.json"];

pub(super) fn plan(b: &mut Build<'_>) {
    let root = b.reader.root().to_owned();
    let config = CONFIG_FILES
        .iter()
        .map(|n| root.join(n))
        .find(|p| p.is_file())
        .and_then(|p| b.json(Component::Config, &p))
        .unwrap_or(Value::Null);
    if let Some(ws) = workspace(b, &config) {
        workspace_files(b, &ws);
        b.skills_in(&ws.join("skills"), false);
        b.skills_in(&ws.join(".agents").join("skills"), false);
    }
    b.skills_in(&root.join("skills"), false);
    let shared = b.options.user_home.join(".agents").join("skills");
    b.skills_in(&shared, false);
    let secrets = if b.options.openclaw.migrate_secrets {
        Secrets::Import
    } else {
        Secrets::Strip
    };
    if let Some(servers) = config.pointer("/mcp/servers").and_then(Value::as_object) {
        b.mcp_servers("mcp.servers", &servers.clone(), secrets);
    }
    if let Some(zone) = config
        .pointer("/agents/defaults/userTimezone")
        .and_then(Value::as_str)
    {
        let e = Entry::new(
            Component::Config,
            "agents.defaults.userTimezone",
            Disposition::Mapped,
        )
        .to("cron.timezone");
        b.key(e, "cron.timezone", zone);
    }
    infrastructure(b, &config);
    if let Some(session) = config.get("session").and_then(Value::as_object) {
        for key in session.keys() {
            b.note(
                Entry::new(Component::Config, format!("session.{key}"), Disposition::Unsupported)
                    .because("Prometheus sessions have no lifetime settings; they end on /new or idle compaction"),
            );
        }
    }
}

/// The workspace directory: `agents.defaults.workspace`, else the first of
/// `workspace/`, `workspace.default/`, `workspace-main/` and
/// `workspace-<agentId>/` that exists.
fn workspace(b: &Build<'_>, config: &Value) -> Option<PathBuf> {
    let root = b.reader.root();
    let configured = config
        .pointer("/agents/defaults/workspace")
        .and_then(Value::as_str)
        .map(|p| expand(p, &b.options.user_home));
    let mut candidates: Vec<PathBuf> = configured.into_iter().collect();
    candidates.extend(
        ["workspace", "workspace.default", "workspace-main"]
            .iter()
            .map(|n| root.join(n)),
    );
    let agents = config.pointer("/agents/list").and_then(Value::as_array);
    for id in agents
        .into_iter()
        .flatten()
        .filter_map(|a| a.get("id").and_then(Value::as_str))
    {
        candidates.push(root.join(format!("workspace-{id}")));
    }
    candidates.into_iter().find(|p| p.is_dir())
}

fn expand(path: &str, user_home: &std::path::Path) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => user_home.join(rest),
        None => PathBuf::from(path),
    }
}

fn workspace_files(b: &mut Build<'_>, ws: &std::path::Path) {
    let soul = ws.join("SOUL.md");
    match b.reader.text(&soul) {
        Ok(Some(text)) if !text.trim().is_empty() => {
            let item = b.item(&soul);
            let e = Entry::new(Component::Identity, item, Disposition::Mapped)
                .to(pmt_config::profiles::IDENTITY_FILE);
            b.file(
                Component::Identity,
                e,
                PathBuf::from(pmt_config::profiles::IDENTITY_FILE),
                text,
            );
        }
        Ok(_) => {}
        Err(why) => b.broken(Component::Identity, b.item(&soul), why),
    }
    b.memory_file(
        &ws.join("MEMORY.md"),
        MemoryTarget::Memory,
        MemoryKind::Fact,
        true,
    );
    b.memory_file(
        &ws.join("USER.md"),
        MemoryTarget::User,
        MemoryKind::Fact,
        true,
    );
    for file in b.reader.files(&ws.join("memory"), &["md"]) {
        b.memory_file(&file, MemoryTarget::Memory, MemoryKind::Fact, true);
    }
    let agents = ws.join("AGENTS.md");
    let item = b.item(&agents);
    match (
        b.reader.text(&agents),
        b.options.openclaw.workspace_target.clone(),
    ) {
        (Ok(Some(text)), Some(target)) => {
            let path = target.join("AGENTS.md");
            let e = Entry::new(Component::Identity, item, Disposition::Mapped)
                .to(path.display().to_string());
            b.file(Component::Identity, e, path, text);
        }
        (Ok(Some(_)), None) => b.note(
            Entry::new(Component::Identity, item, Disposition::Skipped)
                .because("workspace instructions; give --workspace-target to copy AGENTS.md there"),
        ),
        (Ok(None), _) => {}
        (Err(why), _) => b.broken(Component::Identity, item, why),
    }
}

/// Settings `--preset user-data` leaves out.
fn infrastructure(b: &mut Build<'_>, config: &Value) {
    let full = b.options.openclaw.preset == Preset::Full;
    let present: Vec<(&str, &Value)> = [
        ("agents.defaults.model", "/agents/defaults/model"),
        (
            "agents.defaults.thinkingDefault",
            "/agents/defaults/thinkingDefault",
        ),
        (
            "agents.defaults.timeoutSeconds",
            "/agents/defaults/timeoutSeconds",
        ),
        ("agents.defaults.compaction", "/agents/defaults/compaction"),
        ("agents.defaults.sandbox", "/agents/defaults/sandbox"),
        ("tools.exec", "/tools/exec"),
        ("models.providers", "/models/providers"),
        ("channels", "/channels"),
    ]
    .into_iter()
    .filter_map(|(item, pointer)| Some((item, config.pointer(pointer)?)))
    .collect();
    for (item, value) in present {
        if !full {
            b.note(
                Entry::new(Component::Config, item, Disposition::Skipped)
                    .because("infrastructure setting; --preset full imports it"),
            );
            continue;
        }
        match item {
            "agents.defaults.model" => model(b, value),
            "agents.defaults.thinkingDefault" => thinking(b, value),
            "agents.defaults.timeoutSeconds" => timeout(b, value),
            "agents.defaults.compaction" => compaction(b, value),
            "agents.defaults.sandbox" => sandbox(b, value),
            "tools.exec" => exec(b, value),
            "models.providers" => providers(b, value),
            _ => b.note(
                Entry::new(Component::Config, item, Disposition::Unsupported)
                    .because("messaging channels are set up with `prometheus gateway setup`"),
            ),
        }
    }
}

/// `provider/model` split at the first `/`.
fn route(text: &str) -> Option<(&str, &str)> {
    text.split_once('/')
        .filter(|(p, m)| !p.is_empty() && !m.is_empty())
}

fn model(b: &mut Build<'_>, value: &Value) {
    let item = "agents.defaults.model";
    let (primary, fallbacks) = match value {
        Value::String(s) => (Some(s.as_str()), Vec::new()),
        Value::Object(o) => (
            o.get("primary").and_then(Value::as_str),
            o.get("fallbacks")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        ),
        _ => (None, Vec::new()),
    };
    match primary.and_then(route) {
        Some((provider, id)) => {
            let e =
                Entry::new(Component::Config, item, Disposition::Transformed).to("model.default");
            b.key(e, "model.default", id);
            let e = Entry::new(
                Component::Config,
                format!("{item}.provider"),
                Disposition::Transformed,
            )
            .to("model.provider");
            b.key(e, "model.provider", provider);
        }
        None => b.note(
            Entry::new(Component::Config, item, Disposition::Unsupported)
                .because("not a provider/model route"),
        ),
    }
    let chain: Vec<toml_edit::InlineTable> = fallbacks
        .iter()
        .filter_map(Value::as_str)
        .filter_map(route)
        .map(|(p, m)| {
            let mut t = toml_edit::InlineTable::new();
            t.insert("provider", p.into());
            t.insert("model", m.into());
            t
        })
        .collect();
    if !chain.is_empty() {
        let e = Entry::new(
            Component::Config,
            format!("{item}.fallbacks"),
            Disposition::Transformed,
        )
        .to("fallback.chain");
        let array: toml_edit::Array = chain
            .into_iter()
            .map(toml_edit::Value::InlineTable)
            .collect();
        b.key(e, "fallback.chain", toml_edit::Value::Array(array));
    }
}

/// OpenClaw thinking levels to a reasoning effort.
#[must_use]
pub(super) fn effort(level: &str) -> Option<&'static str> {
    match level {
        "always" | "high" | "xhigh" => Some("high"),
        "auto" | "medium" | "adaptive" => Some("medium"),
        "off" | "low" | "none" | "minimal" => Some("low"),
        _ => None,
    }
}

fn thinking(b: &mut Build<'_>, value: &Value) {
    let item = "agents.defaults.thinkingDefault";
    let level = value.as_str().unwrap_or_default();
    match effort(level) {
        Some(e) => {
            let entry = Entry::new(Component::Config, item, Disposition::Transformed)
                .to("agent.reasoning_effort")
                .showing(level)
                .because(format!("thinking level {level} is reasoning effort {e}"));
            b.key(entry, "agent.reasoning_effort", e);
        }
        None => b.note(
            Entry::new(Component::Config, item, Disposition::Unsupported)
                .showing(level)
                .because("unknown thinking level"),
        ),
    }
}

fn timeout(b: &mut Build<'_>, value: &Value) {
    let item = "agents.defaults.timeoutSeconds";
    let Some(secs) = value.as_u64() else {
        return b.note(
            Entry::new(Component::Config, item, Disposition::Unsupported)
                .because("not a number of seconds"),
        );
    };
    let iterations = (secs / 10).clamp(1, 200);
    let e = Entry::new(Component::Config, item, Disposition::Transformed)
        .to("agent.max_iterations")
        .because(format!(
            "{secs} s at about 10 s per model call, capped at 200"
        ));
    b.key(
        e,
        "agent.max_iterations",
        i64::try_from(iterations).unwrap_or(200),
    );
}

fn compaction(b: &mut Build<'_>, value: &Value) {
    let item = "agents.defaults.compaction";
    if let Some(mode) = value.get("mode").and_then(Value::as_str) {
        let enabled = !matches!(mode, "off" | "disabled" | "none");
        let e = Entry::new(
            Component::Config,
            format!("{item}.mode"),
            Disposition::Transformed,
        )
        .to("compression.enabled")
        .showing(mode);
        b.key(e, "compression.enabled", enabled);
    }
    if let Some((provider, id)) = value.get("model").and_then(Value::as_str).and_then(route) {
        let e = Entry::new(
            Component::Config,
            format!("{item}.model"),
            Disposition::Transformed,
        )
        .to("auxiliary.compression.model");
        b.key(e, "auxiliary.compression.model", id);
        let e = Entry::new(
            Component::Config,
            format!("{item}.model.provider"),
            Disposition::Transformed,
        )
        .to("auxiliary.compression.provider");
        b.key(e, "auxiliary.compression.provider", provider);
    }
}

fn sandbox(b: &mut Build<'_>, value: &Value) {
    let item = "agents.defaults.sandbox";
    let backend = value
        .get("backend")
        .and_then(Value::as_str)
        .unwrap_or("docker");
    let mode = value.get("mode").and_then(Value::as_str).unwrap_or("all");
    if backend != "docker" || mode == "off" {
        return b.note(
            Entry::new(Component::Config, item, Disposition::Unsupported)
                .showing(format!("{backend}/{mode}"))
                .because("only an active Docker sandbox maps to the container backend"),
        );
    }
    let e = Entry::new(
        Component::Config,
        format!("{item}.backend"),
        Disposition::Transformed,
    )
    .to("terminal.backend");
    b.key(e, "terminal.backend", "container");
    if let Some(image) = value.pointer("/docker/image").and_then(Value::as_str) {
        let e = Entry::new(
            Component::Config,
            format!("{item}.docker.image"),
            Disposition::Mapped,
        )
        .to("terminal.container.image");
        b.key(e, "terminal.container.image", image);
    }
}

fn exec(b: &mut Build<'_>, value: &Value) {
    let secs = ["timeoutSec", "timeoutSeconds"]
        .iter()
        .find_map(|k| value.get(*k).and_then(Value::as_u64));
    if let Some(secs) = secs {
        let e = Entry::new(
            Component::Config,
            "tools.exec.timeoutSec",
            Disposition::Mapped,
        )
        .to("terminal.timeout_secs");
        b.key(
            e,
            "terminal.timeout_secs",
            i64::try_from(secs).unwrap_or(i64::MAX),
        );
    }
}

/// The wire format of an OpenClaw provider `api`.
fn wire(api: &str) -> Option<&'static str> {
    match api {
        "openai-completions" => Some("chat_completions"),
        "openai-responses" => Some("responses"),
        "anthropic-messages" => Some("anthropic_messages"),
        _ => None,
    }
}

fn providers(b: &mut Build<'_>, value: &Value) {
    let Some(map) = value.as_object() else {
        return;
    };
    for (id, p) in map {
        let item = format!("models.providers.{id}");
        let base = format!("providers.{id}");
        if let Some(url) = p.get("baseUrl").and_then(Value::as_str) {
            let e = Entry::new(
                Component::Config,
                format!("{item}.baseUrl"),
                Disposition::Mapped,
            )
            .to(format!("{base}.base_url"));
            b.key(e, &format!("{base}.base_url"), url);
        }
        if let Some(format) = p.get("api").and_then(Value::as_str).and_then(wire) {
            let e = Entry::new(
                Component::Config,
                format!("{item}.api"),
                Disposition::Transformed,
            )
            .to(format!("{base}.wire_format"));
            b.key(e, &format!("{base}.wire_format"), format);
        }
        if let Some(key) = p.get("apiKey").and_then(Value::as_str) {
            api_key(b, &item, id, key);
        }
    }
}

fn api_key(b: &mut Build<'_>, item: &str, provider: &str, key: &str) {
    let secret: String = format!("{provider}_API_KEY")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    let item = format!("{item}.apiKey");
    let e = Entry::new(Component::Config, &item, Disposition::Secret).to(&secret);
    let key_ref = Entry::new(
        Component::Config,
        format!("{item}:key_secret"),
        Disposition::Transformed,
    )
    .to(format!("providers.{provider}.key_secret"));
    b.key(
        key_ref,
        &format!("providers.{provider}.key_secret"),
        secret.as_str(),
    );
    if b.options.openclaw.migrate_secrets {
        b.secret(e.because("stored in the secrets store"), &secret, key);
    } else {
        b.note(e.because(format!(
            "value not imported; rerun with --migrate-secrets or add it with `prometheus secrets set {secret}`"
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::effort;

    #[test]
    fn thinking_levels_map_to_reasoning_effort() {
        for (level, expected) in [
            ("always", "high"),
            ("xhigh", "high"),
            ("adaptive", "medium"),
            ("auto", "medium"),
            ("off", "low"),
            ("minimal", "low"),
        ] {
            assert_eq!(effort(level), Some(expected), "{level}");
        }
        assert_eq!(effort("weird"), None);
    }
}
