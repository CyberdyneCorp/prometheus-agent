//! Whole-config checks of `[lsp]` (spec: lsp-diagnostics, Built-in server
//! registry; Server overrides and custom servers): a key of
//! `lsp.servers` is a built-in id, or declares a custom server with a
//! non-empty `command` and `extensions`; root globs parse.

use pmt_core::error::{Error, codes};

use crate::schema::{Config, LSP_SERVER_IDS, LspServerConfig};

fn invalid(message: String) -> Error {
    Error::new(&codes::CONFIG_INVALID, message)
}

/// Checks `[lsp]`.
///
/// # Errors
///
/// `config.invalid` naming the key.
pub fn check(config: &Config) -> Result<(), Error> {
    let lsp = &config.lsp;
    for (key, list) in [
        ("exclude_roots", &lsp.exclude_roots),
        ("trusted_workspaces", &lsp.trusted_workspaces),
    ] {
        for (index, pattern) in list.iter().enumerate() {
            if pattern.trim().is_empty() || glob::Pattern::new(pattern).is_err() {
                return Err(invalid(format!(
                    "lsp.{key}[{index}]: '{pattern}' is not a path or glob"
                )));
            }
        }
    }
    for (id, server) in &lsp.servers {
        check_server(id, server)?;
    }
    Ok(())
}

fn check_server(id: &str, server: &LspServerConfig) -> Result<(), Error> {
    if let Some(command) = &server.command
        && command
            .first()
            .is_none_or(|program| program.trim().is_empty())
    {
        return Err(invalid(format!(
            "lsp.servers.{id}.command: the argv must name a program"
        )));
    }
    if LSP_SERVER_IDS.contains(&id) {
        return Ok(());
    }
    if server.command.is_none() || server.extensions.is_empty() {
        return Err(invalid(format!(
            "lsp.servers.{id}: '{id}' is not a built-in language server; a custom server needs `command` and `extensions`"
        ))
        .with_hint(format!(
            "built-in ids: {}; `prometheus lsp list` shows them",
            LSP_SERVER_IDS.join(", ")
        )));
    }
    if let Some(bad) = server.extensions.iter().find(|e| !is_extension(e)) {
        return Err(invalid(format!(
            "lsp.servers.{id}.extensions: '{bad}' is not an extension like '.ml2'"
        )));
    }
    Ok(())
}

fn is_extension(text: &str) -> bool {
    text.strip_prefix('.')
        .is_some_and(|rest| !rest.is_empty() && !rest.contains(['/', '\\', '.']))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn a_custom_server_needs_a_command_and_extensions() {
        let ok = config(
            "[lsp.servers.mylang]\ncommand = [\"mylang-ls\", \"--stdio\"]\nextensions = [\".ml2\"]\n",
        );
        assert!(check(&ok).is_ok());
        let unknown = config("[lsp.servers.pyrite]\ndisabled = true\n");
        let err = check(&unknown).unwrap_err();
        assert_eq!(err.code(), &codes::CONFIG_INVALID);
        assert!(
            err.message().contains("'pyrite' is not a built-in"),
            "{}",
            err.message()
        );
        let bad = config("[lsp.servers.x]\ncommand = [\"x\"]\nextensions = [\"ml2\"]\n");
        assert!(check(&bad).is_err());
    }

    #[test]
    fn a_built_in_override_needs_no_extensions() {
        let ok = config("[lsp.servers.gopls]\ndisabled = true\nenv = { GOFLAGS = \"-mod=mod\" }\n");
        assert!(check(&ok).is_ok());
        let empty = config("[lsp.servers.gopls]\ncommand = []\n");
        assert!(check(&empty).is_err());
    }

    #[test]
    fn a_non_list_exclude_roots_fails_to_load() {
        let err = toml::from_str::<Config>("[lsp]\nexclude_roots = \"~/big\"\n").unwrap_err();
        assert!(err.to_string().contains("exclude_roots"), "{err}");
    }
}
