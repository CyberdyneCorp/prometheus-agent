//! `prometheus --json desktop catalog-entry <kind> <id>` (hidden; spec:
//! desktop-app, Install deep links open a consent flow): what the
//! desktop's install dialog shows about a catalog entry — name,
//! publisher, version, signature status and requested capabilities —
//! read from the configured catalog without downloading the entry.
//! Anything that is not a catalog id is `Unknown catalog entry <id>`.

use pmt_core::error::{Error, codes};
use serde_json::{Value, json};

use crate::build_info;
use crate::context::Context;
use crate::output::json as print_json;

/// What is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Kind {
    /// A plugin of the plugin catalog.
    Plugin,
    /// A skill of the official skill catalog.
    Skill,
}

/// The refusal of anything that is not a catalog entry.
pub(crate) fn unknown(id: &str) -> Error {
    Error::new(&codes::CLI_USAGE, format!("Unknown catalog entry {id}"))
}

pub(super) fn entry(ctx: &Context<'_>, kind: Kind, id: &str) -> Result<u8, Error> {
    let found = match kind {
        Kind::Plugin => plugin(ctx, id)?,
        Kind::Skill => skill(id),
    };
    print_json(&found.ok_or_else(|| unknown(id))?)
}

fn plugin(ctx: &Context<'_>, id: &str) -> Result<Option<Value>, Error> {
    if pmt_plugins::manifest::check_id(id).is_err() {
        return Ok(None);
    }
    let scope = ctx.scope()?;
    // The configured catalog (fetched and verified when its cache is
    // stale): looking an entry up downloads nothing of the entry.
    let catalog = crate::plugins_host::catalog::current(&scope, true);
    let Some(entry) = catalog.find(id) else {
        return Ok(None);
    };
    if catalog.blocked(id, Some(&entry.repo)).is_some() {
        return Ok(None);
    }
    let publisher = entry
        .repo
        .trim_start_matches("https://")
        .split('/')
        .take(2)
        .collect::<Vec<_>>()
        .join("/");
    Ok(Some(json!({
        "kind": "plugin",
        "id": entry.id,
        "name": entry.id,
        "description": entry.description,
        "publisher": format!("{publisher} ({} tier)", entry.tier),
        "version": entry.version.clone().unwrap_or_else(|| entry.r#ref.clone()),
        "signature": "signed: verified against the catalog's publisher key before install",
        "capabilities": [entry.capabilities_summary],
    })))
}

fn skill(id: &str) -> Option<Value> {
    let prefix = id.strip_prefix(pmt_skills::official::OFFICIAL_SOURCE)?;
    let bundled = pmt_skills::bundled::Bundled::official();
    let (name, _) = pmt_skills::official::skills(&bundled)
        .into_iter()
        .find(|(_, p)| p == prefix)?;
    let text = bundled.read(prefix, pmt_skills::layout::SKILL_FILE)?;
    let front =
        pmt_skills::frontmatter::mapping(&String::from_utf8_lossy(text)).unwrap_or_default();
    let field = |key: &str| {
        front
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let tools = front
        .get("allowed-tools")
        .map(|v| match v {
            Value::String(s) => s
                .split([',', ' '])
                .filter(|t| !t.is_empty())
                .map(str::to_owned)
                .collect(),
            Value::Array(a) => a
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        })
        .unwrap_or_default();
    let capabilities: Vec<String> = if tools.is_empty() {
        vec!["instructions only: uses the tools the session already has".to_owned()]
    } else {
        tools.iter().map(|t| format!("tool {t}")).collect()
    };
    Some(json!({
        "kind": "skill",
        "id": id,
        "name": name,
        "description": field("description"),
        "publisher": "Prometheus (official catalog)",
        "version": build_info::VERSION,
        "signature": "embedded in this signed release",
        "capabilities": capabilities,
    }))
}
