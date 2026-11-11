//! The static compatibility audit of a Python plugin (spec: python-bridge,
//! Static compatibility audit; Migration of Hermes plugins).
//!
//! Every `.py` file of the plugin is read ([`super::source`]) for
//! `ctx.<member>` uses outside the supported subset, imports of Hermes
//! internal modules the shim does not provide, direct `os.environ` reads
//! of secret-looking names and hook names outside the registry. A finding
//! is *blocking* when it runs while the plugin loads: at module level
//! (import time) or in `register` and the functions it calls. Blocking
//! findings make the plugin `incompatible`, other findings `degraded`.
//!
//! The same reading infers the capabilities the plugin will ask for
//! (secrets it reads, hosts its URLs name, hooks, commands...), which a
//! `plugin.yaml`-only plugin requests and migration proposes.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use serde::Serialize;

use super::api;
use super::source::Source;

/// What a finding is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Issue {
    /// A `PluginContext` member outside the supported subset.
    UnsupportedApi,
    /// An import of a Hermes internal module.
    InternalImport,
    /// A direct `os.environ` read of a secret name (use `get_secret`).
    SecretEnvRead,
    /// A hook name with no Prometheus equivalent.
    UnknownHook,
}

/// One finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// What it is about.
    pub issue: Issue,
    /// The member, module, variable or hook name.
    pub name: String,
    /// The file, relative to the plugin directory.
    pub file: String,
    /// 1-based line.
    pub line: usize,
    /// Runs while the plugin loads.
    pub blocking: bool,
}

/// The audit's classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// No findings.
    Compatible,
    /// Only findings outside the load path.
    Degraded,
    /// A finding on the load path.
    Incompatible,
}

impl Verdict {
    /// The report spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compatible => "compatible",
            Self::Degraded => "degraded",
            Self::Incompatible => "incompatible",
        }
    }
}

/// Capabilities the source suggests.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct Inferred {
    /// Secret names read (`os.environ`, `get_secret`).
    pub secrets: BTreeSet<String>,
    /// Hosts of URL literals.
    pub hosts: BTreeSet<String>,
    /// `register_hook` names.
    pub hooks: BTreeSet<String>,
    /// `register_command` names.
    pub commands: BTreeSet<String>,
    /// `register_cli_command` names.
    pub cli_commands: BTreeSet<String>,
    /// Provider profile names.
    pub providers: BTreeSet<String>,
    /// `platform_actions.<verb>` uses.
    pub platform_actions: BTreeSet<String>,
    /// Registers a system-prompt section.
    pub prompt_section: bool,
    /// Injects messages.
    pub inject_messages: bool,
    /// Registers an approval transport.
    pub approval_transport: bool,
    /// Starts processes.
    pub subprocess: bool,
    /// Calls the model.
    pub llm: bool,
    /// Registers UI catalogs (`register_locale`, `register_locale_dir`):
    /// its `locales/` YAML files are converted at install time.
    pub registers_locales: bool,
    /// A memory provider whose `cli.py` defines `register_cli(subparser)`:
    /// its command is named after the plugin.
    pub memory_cli: bool,
}

/// The audit of one plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    /// The classification.
    pub verdict: Verdict,
    /// Every finding, by file and line.
    pub findings: Vec<Finding>,
    /// Suggested capabilities.
    pub inferred: Inferred,
}

impl Report {
    /// The names behind the blocking findings (what migration cites).
    #[must_use]
    pub fn blocking_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .findings
            .iter()
            .filter(|f| f.blocking)
            .map(|f| f.name.clone())
            .collect();
        names.dedup();
        names
    }
}

/// Audits the plugin in `dir` (`extra_hooks`: hook names its manifest
/// declares).
///
/// # Errors
///
/// `plugins.discovery` when a file cannot be read.
pub fn audit(dir: &Path, extra_hooks: &[String]) -> Result<Report, Error> {
    let mut files = Vec::new();
    collect(dir, &mut files)?;
    files.sort();
    let mut read = Vec::new();
    for path in files {
        let text =
            std::fs::read(&path).map_err(|e| Error::io(&codes::PLUGINS_DISCOVERY, &path, &e))?;
        let relative = path.strip_prefix(dir).unwrap_or(&path);
        let name = relative.to_string_lossy().replace('\\', "/");
        read.push((name, Source::read(&String::from_utf8_lossy(&text))));
    }
    let reachable = reachable(&read);
    let mut findings = Vec::new();
    let mut inferred = Inferred::default();
    for (index, (file, source)) in read.iter().enumerate() {
        let file_reach = &reachable[index];
        scan(file, source, file_reach, &mut findings, &mut inferred);
    }
    inferred.memory_cli = memory_cli(&read);
    for hook in extra_hooks {
        inferred.hooks.insert(hook.clone());
        if pmt_core::hooks::check(hook).is_err() {
            findings.push(Finding {
                issue: Issue::UnknownHook,
                name: hook.clone(),
                file: super::hermes::HERMES_MANIFEST.to_owned(),
                line: 0,
                blocking: true,
            });
        }
    }
    let verdict = if findings.iter().any(|f| f.blocking) {
        Verdict::Incompatible
    } else if findings.is_empty() {
        Verdict::Compatible
    } else {
        Verdict::Degraded
    };
    Ok(Report {
        verdict,
        findings,
        inferred,
    })
}

/// Whether a memory-provider plugin has a top-level `cli.py` defining
/// `register_cli`, the Hermes memory-provider command.
fn memory_cli(files: &[(String, Source)]) -> bool {
    let has =
        |file: &(String, Source), needle: &str| file.1.code.iter().any(|l| l.contains(needle));
    files
        .iter()
        .any(|f| f.0 == "cli.py" && has(f, "def register_cli("))
        && files.iter().any(|f| has(f, "register_memory_provider("))
}

/// Every `.py` file under `dir` (caches and virtual environments skipped).
fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Error> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| Error::io(&codes::PLUGINS_DISCOVERY, dir, &e))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir()
            && !matches!(
                name.as_ref(),
                "__pycache__" | ".venv" | "venv" | "node_modules"
            )
            && !name.starts_with('.')
        {
            collect(&path, out)?;
        } else if kind.is_file() && name.ends_with(".py") {
            out.push(path);
        }
    }
    Ok(())
}

/// Per file, which `def`s run while the plugin loads: `register` and every
/// function it (transitively) calls by name, in any file.
fn reachable(files: &[(String, Source)]) -> Vec<HashSet<usize>> {
    let mut names: BTreeSet<String> = BTreeSet::from(["register".to_owned()]);
    let mut frontier: Vec<String> = names.iter().cloned().collect();
    while let Some(name) = frontier.pop() {
        for called in calls_of(files, &name) {
            if names.insert(called.clone()) {
                frontier.push(called);
            }
        }
    }
    files
        .iter()
        .map(|(_, source)| {
            source
                .defs
                .iter()
                .enumerate()
                .filter(|(_, d)| d.parent.is_none() && names.contains(&d.name))
                .map(|(i, _)| i)
                .collect()
        })
        .collect()
}

/// What the module-level functions named `name` call, in every file.
fn calls_of(files: &[(String, Source)], name: &str) -> Vec<String> {
    files
        .iter()
        .flat_map(|(_, source)| &source.defs)
        .filter(|d| d.name == name && d.parent.is_none())
        .flat_map(|d| d.calls.iter().cloned())
        .collect()
}

/// Whether line `at` runs while the plugin loads.
fn blocking(source: &Source, reach: &HashSet<usize>, at: usize) -> bool {
    source.owner[at].is_none_or(|def| reach.contains(&def))
}

/// Where a finding of one line goes.
struct Line<'a> {
    file: &'a str,
    source: &'a Source,
    reach: &'a HashSet<usize>,
    at: usize,
}

impl Line<'_> {
    fn finding(&self, issue: Issue, name: String) -> Finding {
        Finding {
            issue,
            name,
            file: self.file.to_owned(),
            line: self.at + 1,
            blocking: blocking(self.source, self.reach, self.at) && issue != Issue::SecretEnvRead,
        }
    }
}

fn scan(
    file: &str,
    source: &Source,
    reach: &HashSet<usize>,
    findings: &mut Vec<Finding>,
    inferred: &mut Inferred,
) {
    let contexts = context_names(source);
    for at in 0..source.code.len() {
        let line = Line {
            file,
            source,
            reach,
            at,
        };
        scan_members(&line, &contexts, findings, inferred);
        scan_imports(&line, findings, inferred);
        scan_env(&line, findings, inferred);
        infer_line(source, at, inferred);
        for hook in hook_names(source, at) {
            if pmt_core::hooks::check(&hook).is_err() {
                findings.push(line.finding(Issue::UnknownHook, hook.clone()));
            }
            inferred.hooks.insert(hook);
        }
    }
    infer_providers(&source.raw.join("\n"), inferred);
}

fn scan_members(
    line: &Line<'_>,
    contexts: &[String],
    findings: &mut Vec<Finding>,
    inferred: &mut Inferred,
) {
    for (member, column) in members(&line.source.code[line.at], contexts) {
        infer_member(line.source, line.at, column, &member, inferred);
        if !api::is_supported(&member) {
            findings.push(line.finding(Issue::UnsupportedApi, member));
        }
    }
}

fn scan_imports(line: &Line<'_>, findings: &mut Vec<Finding>, inferred: &mut Inferred) {
    for module in imports(&line.source.code[line.at]) {
        if module == "subprocess" {
            inferred.subprocess = true;
        }
        if api::is_refused_import(&module) {
            findings.push(line.finding(Issue::InternalImport, module));
        }
    }
}

fn scan_env(line: &Line<'_>, findings: &mut Vec<Finding>, inferred: &mut Inferred) {
    for name in env_reads(&line.source.raw[line.at], &line.source.code[line.at]) {
        inferred.secrets.insert(name.clone());
        if looks_secret(&name) {
            findings.push(line.finding(Issue::SecretEnvRead, name));
        }
    }
}

/// The names a `PluginContext` goes by: `ctx` and each `register`'s first
/// parameter.
fn context_names(source: &Source) -> Vec<String> {
    let mut names = vec!["ctx".to_owned()];
    names.extend(
        source
            .defs
            .iter()
            .filter(|d| d.name == "register")
            .filter_map(|d| d.first_param.clone()),
    );
    // Aliases: `_CTX = ctx` keeps the context for handlers.
    let mut aliases = Vec::new();
    for line in &source.code {
        if let Some((left, right)) = line.trim().split_once('=')
            && names.iter().any(|n| n == right.trim())
            && left.trim().chars().all(|c| c.is_alphanumeric() || c == '_')
            && !left.trim().is_empty()
        {
            aliases.push(left.trim().to_owned());
        }
    }
    names.extend(aliases);
    names.sort();
    names.dedup();
    names
}

/// `<ctx>.<member>` uses on a code line, with the column after the member.
fn members(code: &str, contexts: &[String]) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    for context in contexts {
        let mut from = 0;
        while let Some(found) = code[from..].find(context.as_str()) {
            let start = from + found;
            from = start + context.len();
            let before_ok = start == 0 || !is_ident(code.as_bytes()[start - 1]);
            let rest = &code[from..];
            let Some(after_dot) = rest.trim_start().strip_prefix('.').filter(|_| before_ok) else {
                continue;
            };
            let member: String = after_dot
                .trim_start()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !member.is_empty() {
                let column = code.len() - after_dot.trim_start().len() + member.len();
                out.push((member, column));
            }
        }
    }
    out
}

fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Modules a code line imports (`import a.b, c`, `from a.b import x`;
/// relative imports skipped).
fn imports(code: &str) -> Vec<String> {
    let text = code.trim();
    if let Some(rest) = text.strip_prefix("import ") {
        return rest
            .split(',')
            .filter_map(|part| part.split_whitespace().next())
            .map(str::to_owned)
            .collect();
    }
    if let Some(rest) = text.strip_prefix("from ") {
        let module = rest.split_whitespace().next().unwrap_or("");
        if !module.is_empty() && !module.starts_with('.') && rest.contains(" import") {
            return vec![module.to_owned()];
        }
    }
    Vec::new()
}

/// Variable names read through `os.environ.get("X")`, `os.environ["X"]`
/// or `os.getenv("X")` in code (not comments or strings).
fn env_reads(raw: &str, code: &str) -> Vec<String> {
    let mut out = Vec::new();
    for pattern in [
        "os.environ.get(",
        "os.environ[",
        "os.getenv(",
        "environ.get(",
    ] {
        let mut from = 0;
        while let Some(found) = code[from..].find(pattern) {
            let start = from + found;
            from = start + pattern.len();
            if pattern == "environ.get(" && code[..start].ends_with("os.") {
                continue;
            }
            let rest = raw[from..].trim_start();
            if let Some(name) = quoted(rest).filter(|n| is_env_name(n)) {
                out.push(name);
            }
        }
    }
    out
}

/// The text of the string literal `text` starts with.
fn quoted(text: &str) -> Option<String> {
    let quote = text.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let body = &text[1..];
    body.find(quote).map(|end| body[..end].to_owned())
}

fn is_env_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Whether a variable name looks like a credential.
fn looks_secret(name: &str) -> bool {
    [
        "KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "AUTH",
    ]
    .iter()
    .any(|word| name.contains(word))
}

/// The first string argument after `<call>(` on line `at`, keyword or
/// positional.
fn first_literal(source: &Source, at: usize, call: &str) -> Option<String> {
    let code = &source.code[at];
    let start = code.find(&format!("{call}("))? + call.len() + 1;
    let rest = source.raw[at].get(start..)?.trim_start();
    let rest = rest.strip_prefix("name").map_or(rest, |r| {
        r.trim_start().trim_start_matches('=').trim_start()
    });
    quoted(rest)
}

/// Hook names `register_hook("...")` registers on line `at`.
fn hook_names(source: &Source, at: usize) -> Vec<String> {
    first_literal(source, at, "register_hook")
        .into_iter()
        .collect()
}

/// What a supported member suggests.
fn infer_member(source: &Source, at: usize, column: usize, member: &str, inferred: &mut Inferred) {
    match member {
        "register_command" => {
            inferred
                .commands
                .extend(first_literal(source, at, "register_command"));
        }
        "register_cli_command" => {
            inferred
                .cli_commands
                .extend(first_literal(source, at, "register_cli_command"));
        }
        "register_system_prompt_section" => inferred.prompt_section = true,
        "register_locale" | "register_locale_dir" => inferred.registers_locales = true,
        "inject_message" => inferred.inject_messages = true,
        "register_approval_transport" => inferred.approval_transport = true,
        "llm" => inferred.llm = true,
        "platform_actions" => {
            let rest = source.code[at].get(column..).unwrap_or("").trim_start();
            if let Some(verb) = rest.strip_prefix('.') {
                let verb: String = verb
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                inferred.platform_actions.insert(verb);
            }
        }
        _ => {}
    }
}

/// Secrets, hosts and subprocess use on line `at`.
fn infer_line(source: &Source, at: usize, inferred: &mut Inferred) {
    let code = &source.code[at];
    for call in ["get_secret", "get_secret_str"] {
        if let Some(name) = first_literal(source, at, call).filter(|n| is_env_name(n)) {
            inferred.secrets.insert(name);
        }
    }
    if ["os.system(", "Popen(", "subprocess.", "os.spawn"]
        .iter()
        .any(|p| code.contains(p))
    {
        inferred.subprocess = true;
    }
    let raw = &source.raw[at];
    for scheme in ["https://", "http://"] {
        let mut from = 0;
        while let Some(found) = raw[from..].find(scheme) {
            let start = from + found + scheme.len();
            from = start;
            let host: String = raw[start..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
                .collect();
            if host.contains('.') && !host.starts_with('.') && !host.ends_with('.') {
                inferred.hosts.insert(host.to_ascii_lowercase());
            }
        }
    }
}

/// `name="..."` in the arguments of every `...Profile(` and
/// `register_provider(` call (model-provider plugins).
fn infer_providers(text: &str, inferred: &mut Inferred) {
    for marker in ["Profile(", "register_provider("] {
        let mut from = 0;
        while let Some(found) = text[from..].find(marker) {
            let start = from + found + marker.len();
            from = start;
            let args = &text[start..start + balanced_end(&text[start..])];
            if let Some(name) = keyword_literal(args, "name") {
                inferred.providers.insert(name);
            }
        }
    }
}

/// The offset of the `)` closing an argument list that `text` starts.
fn balanced_end(text: &str) -> usize {
    let mut depth = 1;
    for (offset, c) in text.char_indices() {
        depth += match c {
            '(' => 1,
            ')' => -1,
            _ => 0,
        };
        if depth == 0 {
            return offset;
        }
    }
    text.len()
}

/// The string literal given to keyword `key` in `args`.
fn keyword_literal(args: &str, key: &str) -> Option<String> {
    let mut from = 0;
    while let Some(found) = args[from..].find(key) {
        let start = from + found;
        from = start + key.len();
        let before_ok = start == 0 || !is_ident(args.as_bytes()[start - 1]);
        let rest = args[from..].trim_start();
        if let (true, Some(value)) = (before_ok, rest.strip_prefix('=')) {
            return quoted(value.trim_start());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            std::fs::write(dir.path().join(name), text).unwrap();
        }
        dir
    }

    #[test]
    fn an_unsupported_call_from_a_handler_degrades_with_file_and_line() {
        let dir = plugin(&[(
            "__init__.py",
            "def _handle(args, **kw):\n    ctx.register_telegram_handler(lambda: None)\n\ndef register(ctx):\n    ctx.register_tool(name='x', toolset='x', schema={}, handler=_handle)\n",
        )]);
        let report = audit(dir.path(), &[]).unwrap();
        assert_eq!(report.verdict, Verdict::Degraded);
        let finding = &report.findings[0];
        assert_eq!(finding.name, "register_telegram_handler");
        assert_eq!((finding.file.as_str(), finding.line), ("__init__.py", 2));
    }

    #[test]
    fn an_unsupported_call_from_register_is_incompatible() {
        let dir = plugin(&[(
            "__init__.py",
            "def register(ctx):\n    setup(ctx)\n\ndef setup(c):\n    ctx = c\n    ctx.register_platform('x')\n",
        )]);
        let report = audit(dir.path(), &[]).unwrap();
        assert_eq!(report.verdict, Verdict::Incompatible);
        assert_eq!(report.blocking_names(), ["register_platform"]);
    }

    #[test]
    fn internal_imports_and_secret_reads_are_reported() {
        let dir = plugin(&[(
            "__init__.py",
            "import os\nfrom gateway.run import GatewayRunner\nfrom agent.memory_provider import MemoryProvider\nKEY = os.environ.get(\"SPOTIFY_TOKEN\")\nURL = 'https://api.spotify.com/v1'\n# os.environ.get(\"OTHER_KEY\")\ndef register(ctx):\n    ctx.register_hook('post_tool_call', print)\n",
        )]);
        let report = audit(dir.path(), &[]).unwrap();
        let names: Vec<&str> = report.findings.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["gateway.run", "SPOTIFY_TOKEN"]);
        assert_eq!(report.verdict, Verdict::Incompatible);
        assert!(report.inferred.secrets.contains("SPOTIFY_TOKEN"));
        assert!(!report.inferred.secrets.contains("OTHER_KEY"));
        assert!(report.inferred.hosts.contains("api.spotify.com"));
        assert!(report.inferred.hooks.contains("post_tool_call"));
    }

    #[test]
    fn a_provider_profile_name_is_inferred() {
        let dir = plugin(&[(
            "__init__.py",
            "from providers import register_provider\nfrom providers.base import ProviderProfile\nclass P(ProviderProfile):\n    pass\nregister_provider(P(\n    name=\"copilot\", base_url=\"https://api.githubcopilot.com\"))\n",
        )]);
        let report = audit(dir.path(), &[]).unwrap();
        assert_eq!(report.verdict, Verdict::Compatible);
        assert!(report.inferred.providers.contains("copilot"));
    }

    #[test]
    fn a_declared_unknown_hook_is_incompatible() {
        let dir = plugin(&[("__init__.py", "def register(ctx):\n    pass\n")]);
        let report = audit(dir.path(), &["on_banana".to_owned()]).unwrap();
        assert_eq!(report.verdict, Verdict::Incompatible);
    }
}
