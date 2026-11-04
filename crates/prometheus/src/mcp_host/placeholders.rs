//! `${NAME}` placeholders of MCP server entries (spec: plugin-system, MCP
//! catalog entries).
//!
//! Filled at each start, never written back: from the context variables
//! `${userHome}`, `${workspaceFolder}`, `${workspaceFolderBasename}` and
//! `${pathSeparator}`, otherwise from the secrets store (a setup field a
//! catalog install stored there). Nothing else (no environment variable)
//! can fill one, and an unknown name fails the start naming it.

use std::path::Path;

use pmt_config::ProfileScope;

/// `text` with its placeholders filled.
///
/// # Errors
///
/// An unknown placeholder, or one left unterminated.
pub(super) fn fill(scope: &ProfileScope, workspace: &Path, text: &str) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return Err(format!("unterminated placeholder in '{text}'"));
        };
        out.push_str(&value(scope, workspace, &after[..end])?);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn value(scope: &ProfileScope, workspace: &Path, name: &str) -> Result<String, String> {
    let context = match name {
        "userHome" => Some(
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|h| h.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ),
        "workspaceFolder" => Some(workspace.to_string_lossy().into_owned()),
        "workspaceFolderBasename" => Some(
            workspace
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ),
        "pathSeparator" => Some(std::path::MAIN_SEPARATOR.to_string()),
        _ => None,
    };
    if let Some(value) = context {
        return Ok(value);
    }
    pmt_config::secrets::surface::SurfaceOnly::of(scope.config())
        .check(name)
        .map_err(|e| e.message().to_owned())?;
    scope
        .secrets()
        .resolve(name)
        .map(|secret| secret.expose().to_owned())
        .map_err(|_| {
            format!(
                "${{{name}}} is neither a context variable nor a stored secret (set it with: prometheus secrets set {name})"
            )
        })
}
