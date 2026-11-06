//! `prometheus config fix-models` (spec: configuration-and-profiles,
//! Retired model rewrite): lists every config key naming a deprecated or
//! retired model with its replacement; only `--apply` rewrites them,
//! through the format-preserving writer, after copying `config.toml` to
//! `config.toml.bak-<timestamp>` unless `--no-backup`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use pmt_providers::retirement::{self, Finding};

use super::Context;
use crate::output::{document, print_raw};

pub(super) fn run(ctx: &Context<'_>, apply: bool, no_backup: bool) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let (registry, _) = crate::plugins_host::providers::registry(&scope, scope.config())?;
    let rewrite = retirement::scan(scope.config(), &registry);
    let file = scope.profile().config_file();
    let writes = !rewrite.sets.is_empty() || rewrite.chain.is_some();
    let mut backup = None;
    if apply && writes {
        if !no_backup {
            backup = Some(copy_aside(file.path())?);
        }
        let pairs: Vec<(&str, &str)> = rewrite
            .sets
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        if !pairs.is_empty() {
            file.set_all(&pairs)?;
        }
        if let Some(chain) = &rewrite.chain {
            file.set_fallback_chain(chain)?;
        }
    }
    let applied = apply && writes;
    if ctx.json {
        return document(
            "config fix-models",
            &serde_json::json!({
                "findings": rewrite.findings,
                "applied": applied,
                "backup": backup.as_ref().map(|p| p.display().to_string()),
            }),
        );
    }
    print_raw(&render(
        &rewrite.findings,
        apply,
        applied,
        backup.as_deref(),
    ))
}

fn render(findings: &[Finding], apply: bool, applied: bool, backup: Option<&Path>) -> String {
    if findings.is_empty() {
        return "no deprecated or retired model ids in config.toml\n".to_owned();
    }
    let mut text = String::new();
    for f in findings {
        let to = f.to.as_deref().unwrap_or("<no replacement known>");
        let _ = writeln!(
            text,
            "{}: {} -> {to}  ({} {})",
            f.key,
            f.from,
            f.status.as_str(),
            f.id
        );
    }
    if let Some(path) = backup {
        let _ = writeln!(text, "previous config.toml saved as {}", path.display());
    }
    text.push_str(match (apply, applied) {
        (true, true) => "config.toml rewritten\n",
        (true, false) => "nothing to rewrite: no finding has a known replacement\n",
        (false, _) => "nothing changed; run with --apply to rewrite these keys\n",
    });
    text
}

/// Copies `config.toml` to `config.toml.bak-<UTC timestamp>` next to it.
fn copy_aside(path: &Path) -> Result<PathBuf, Error> {
    let stamp = jiff::Timestamp::now().strftime("%Y%m%dT%H%M%SZ");
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(format!(".bak-{stamp}"));
    let backup = path.with_file_name(name);
    std::fs::copy(path, &backup)
        .map_err(|e| Error::io_doing(&codes::HOME_IO, "cannot back up", path, &e))?;
    Ok(backup)
}
