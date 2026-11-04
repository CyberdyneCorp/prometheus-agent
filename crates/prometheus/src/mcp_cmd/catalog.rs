//! `prometheus mcp catalog` and `prometheus mcp install <entry>` (spec:
//! plugin-system, MCP catalog entries; mcp CLI).
//!
//! An install writes `[mcp.servers.<name>]` from the entry, disabled until
//! the person enables it, and never updates it later. Setup fields are
//! asked (or given with `--field NAME=VALUE`): a secret one goes to the
//! secrets store and stays a `${NAME}` placeholder filled at connect time;
//! another is written into the entry. A git `source` is checked out at its
//! pinned commit under `<profile>/mcp/<name>` (`${installDir}` in the
//! entry) and its `bootstrap` commands run there in the sandbox. Tools:
//! `tools.default_excluded` is written as `tools.exclude` without a
//! checklist; otherwise the server is probed and the checklist pre-checks
//! the prior selection, else `tools.default_enabled`, else every tool
//! (non-interactively the pre-check is written as is). A probe that fails
//! still installs, with the defaults.

use std::collections::BTreeMap;
use std::path::Path;

use clap::Args;
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_plugins::mcp_catalog::McpEntry;

use super::json;
use crate::context::Context;
use crate::setup_cmd::Wizard;

/// `mcp install` flags.
#[derive(Debug, Args)]
pub struct InstallArgs {
    /// The catalog entry.
    pub entry: String,
    /// The server name (default: the entry id).
    #[arg(long)]
    pub name: Option<String>,
    /// A setup field's value (`NAME=VALUE`; repeat).
    #[arg(long = "field", value_name = "NAME=VALUE")]
    pub fields: Vec<String>,
}

pub(super) fn list(ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let catalog = crate::plugins_host::catalog::current(&scope, true);
    if ctx.json {
        let entries: Vec<serde_json::Value> = catalog
            .mcp
            .iter()
            .map(|e| {
                serde_json::json!({
                    "id": e.id,
                    "description": e.description,
                    "transport": e.transport,
                    "auth": e.auth,
                    "requires_newer": e.needs_newer(),
                })
            })
            .collect();
        println!("{}", serde_json::json!({"entries": entries}));
        return Ok(0);
    }
    if catalog.mcp.is_empty() {
        println!("the catalog has no MCP servers");
    }
    for entry in &catalog.mcp {
        let newer = if entry.needs_newer() {
            "  (requires a newer Prometheus)"
        } else {
            ""
        };
        println!(
            "{}  {}  auth {}  {}{newer}",
            entry.id, entry.transport, entry.auth, entry.description
        );
    }
    Ok(0)
}

pub(super) fn install(ctx: &Context<'_>, args: &InstallArgs) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let catalog = crate::plugins_host::catalog::current(&scope, true);
    let Some(entry) = catalog.find_mcp(&args.entry).cloned() else {
        return Err(Error::new(
            &codes::MCP_NOT_FOUND,
            format!("the catalog has no MCP server '{}'", args.entry),
        )
        .with_hint("see the entries with: prometheus mcp catalog"));
    };
    if entry.needs_newer() {
        return Err(Error::new(
            &codes::CONFIG_INVALID,
            format!(
                "MCP catalog entry '{}' requires a newer Prometheus (manifest version {})",
                entry.id, entry.manifest_version
            ),
        )
        .with_hint("update with: prometheus update"));
    }
    let name = args.name.clone().unwrap_or_else(|| entry.id.clone());
    if !pmt_config::mcp_check::is_server_name(&name) {
        return Err(Error::new(
            &codes::CLI_USAGE,
            format!("invalid MCP server name '{name}'"),
        ));
    }
    let mut wizard = Wizard::stdio();
    let mut values = setup(&scope, &entry, args, &mut wizard)?;
    let install_dir = scope.dir().join("mcp").join(&name);
    if let Some(source) = &entry.source {
        checkout(&scope, source, &install_dir)?;
    }
    values.insert(
        "installDir".to_owned(),
        install_dir.to_string_lossy().into_owned(),
    );
    let prior = scope
        .config()
        .mcp
        .servers
        .get(&name)
        .map(|s| s.tools.clone());
    write_entry(&scope, &name, &entry, &values)?;
    choose_tools(ctx, &name, &entry, prior, &mut wizard)?;
    println!(
        "installed MCP server '{name}' (disabled); enable it with: prometheus config set mcp.servers.{name}.enabled true"
    );
    if !entry.post_install.is_empty() {
        println!("{}", entry.post_install);
    }
    Ok(0)
}

/// The setup values that are written into the entry (secret ones go to
/// the secrets store).
fn setup(
    scope: &ProfileScope,
    entry: &McpEntry,
    args: &InstallArgs,
    wizard: &mut Wizard<'_>,
) -> Result<BTreeMap<String, String>, Error> {
    let given: BTreeMap<&str, &str> = args
        .fields
        .iter()
        .filter_map(|p| p.split_once('='))
        .collect();
    let mut values = BTreeMap::new();
    for field in &entry.setup {
        let value = match given.get(field.name.as_str()) {
            Some(value) => Some((*value).to_owned()),
            None if field.secret => wizard
                .ask_secret(&format!("{} ({})", field.name, field.prompt))?
                .map(|s| s.expose().to_owned()),
            None => wizard.ask(&format!("{}:", field.prompt))?,
        }
        .or_else(|| field.default.clone());
        let Some(value) = value else {
            return Err(Error::new(
                &codes::CONFIG_INVALID,
                format!("MCP catalog entry '{}' needs {}", entry.id, field.name),
            )
            .with_hint(format!("give it with --field {}=<value>", field.name)));
        };
        if field.secret {
            pmt_config::secrets::check_name(&field.name)?;
            scope
                .secrets()
                .store()
                .set(&field.name, &pmt_core::secret::SecretString::new(value))?;
        } else {
            values.insert(field.name.clone(), value);
        }
    }
    Ok(values)
}

/// `text` with the non-secret `${NAME}` placeholders replaced.
fn fill(text: &str, values: &BTreeMap<String, String>) -> String {
    values.iter().fold(text.to_owned(), |text, (name, value)| {
        text.replace(&format!("${{{name}}}"), value)
    })
}

fn write_entry(
    scope: &ProfileScope,
    name: &str,
    entry: &McpEntry,
    values: &BTreeMap<String, String>,
) -> Result<(), Error> {
    let key = |field: &str| format!("mcp.servers.{name}.{field}");
    let file = scope.profile().config_file();
    if scope.config().mcp.servers.contains_key(name) {
        file.unset(&format!("mcp.servers.{name}"))?;
    }
    let mut pairs = vec![
        (key("enabled"), "false".to_owned()),
        (key("transport"), json(&entry.transport)),
    ];
    if entry.transport == "http" {
        pairs.push((key("url"), json(&fill(&entry.url, values))));
        for (header, value) in &entry.headers {
            pairs.push((
                key(&format!("headers.{header}")),
                json(&fill(value, values)),
            ));
        }
        if matches!(entry.auth.as_str(), "oauth" | "header") {
            pairs.push((key("auth"), json(&entry.auth)));
        }
    } else {
        pairs.push((key("command"), json(&fill(&entry.command, values))));
        let words: Vec<String> = entry.args.iter().map(|a| fill(a, values)).collect();
        if !words.is_empty() {
            pairs.push((key("args"), json(&words)));
        }
        for (var, value) in &entry.env {
            pairs.push((key(&format!("env.{var}")), json(&fill(value, values))));
        }
    }
    if let Some(excluded) = &entry.tools.default_excluded {
        pairs.push((key("tools.exclude"), json(excluded)));
    }
    let borrowed: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    file.set_all(&borrowed)
}

/// The checklist (or the defaults), after a probe.
fn choose_tools(
    ctx: &Context<'_>,
    name: &str,
    entry: &McpEntry,
    prior: Option<pmt_config::schema::McpToolFilter>,
    wizard: &mut Wizard<'_>,
) -> Result<(), Error> {
    if entry.tools.default_excluded.is_some() {
        return Ok(());
    }
    let scope = ctx.scope()?;
    let key = format!("mcp.servers.{name}");
    let file = scope.profile().config_file();
    let preset =
        match prior.filter(|p| p.include.is_some() || !p.exclude.is_empty()) {
            Some(prior) => Some(prior),
            None => entry.tools.default_enabled.clone().map(|include| {
                pmt_config::schema::McpToolFilter {
                    include: Some(include),
                    exclude: Vec::new(),
                }
            }),
        };
    if let Some(filter) = &preset {
        if let Some(include) = &filter.include {
            file.set_all(&[(&format!("{key}.tools.include"), &json(include))])?;
        }
        if !filter.exclude.is_empty() {
            file.set_all(&[(&format!("{key}.tools.exclude"), &json(&filter.exclude))])?;
        }
    }
    if !wizard.is_tty() {
        return Ok(());
    }
    // Probe with the entry enabled for this process only.
    file.set_all(&[(&format!("{key}.enabled"), "true")])?;
    let probed = super::tools::probe_and_choose(ctx, name, wizard);
    file.set_all(&[(&format!("{key}.enabled"), "false")])?;
    probed
}

/// Checks `source` out at its pinned commit into `dir` and runs its
/// bootstrap commands there in the sandbox.
fn checkout(
    scope: &ProfileScope,
    source: &pmt_plugins::mcp_catalog::Source,
    dir: &Path,
) -> Result<(), Error> {
    if dir.exists() {
        std::fs::remove_dir_all(dir).map_err(|e| Error::io(&codes::PLUGINS_CATALOG, dir, &e))?;
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::io(&codes::PLUGINS_CATALOG, parent, &e))?;
    }
    crate::plugins_cmd::install::git_checkout(&source.repo, &source.r#ref, dir)?;
    for command in &source.bootstrap {
        bootstrap(scope, dir, command)?;
    }
    Ok(())
}

fn bootstrap(scope: &ProfileScope, dir: &Path, command: &str) -> Result<(), Error> {
    use pmt_core::capability::{Capability, CapabilitySet};
    let config = scope.config();
    let extra = [
        "process.spawn:*".to_owned(),
        "net.connect:*".to_owned(),
        format!("fs.write:{}/**", dir.display()),
    ];
    let declared: CapabilitySet = extra
        .iter()
        .map(|c| Capability::parse(c))
        .collect::<Result<_, _>>()?;
    let capabilities = pmt_config::grants::baseline(&config.security).union(&declared);
    let paths = pmt_sandbox::Paths {
        user_home: std::env::var_os("HOME").map(Into::into).unwrap_or_default(),
        prometheus_home: scope.home().to_path_buf(),
        workspace: dir.to_path_buf(),
        scratch: vec![dir.to_path_buf()],
    };
    let sandbox = pmt_sandbox::Sandbox::new(
        pmt_sandbox::Policy::from_set(&capabilities, &paths),
        &config.security.sandbox,
    )?;
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let mut process = sandbox.command(shell)?;
    process.arg(flag).arg(command).current_dir(dir);
    let output = process.output().map_err(|e| {
        Error::new(
            &codes::PLUGINS_CATALOG,
            format!("bootstrap '{command}': {e}"),
        )
    })?;
    if !output.status.success() {
        return Err(Error::new(
            &codes::PLUGINS_CATALOG,
            format!(
                "bootstrap '{command}' failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    println!("bootstrap: {command}");
    Ok(())
}
