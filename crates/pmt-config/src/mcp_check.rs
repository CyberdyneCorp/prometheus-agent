//! Whole-config checks of `[mcp]` (spec: plugin-system, MCP client servers;
//! tool-system, MCP tool naming and filtering): server names fit tool and
//! toolset names, each transport has what it needs, secret names are
//! valid, sandbox paths are absolute or under `$workspace`, network grants
//! name hosts and tool filters are valid globs.

use pmt_core::error::{Error, codes};

use crate::schema::{Config, McpServerConfig, McpTransport};

fn invalid(message: String) -> Error {
    Error::new(&codes::CONFIG_INVALID, message)
}

/// The longest MCP server name.
pub const MAX_SERVER_NAME: usize = 64;

/// Whether `name` is a valid `mcp.servers.<name>` key: 1 to 64 ASCII
/// letters, digits, `_` or `-`.
#[must_use]
pub fn is_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SERVER_NAME
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

/// Checks `[mcp]`.
///
/// # Errors
///
/// `config.invalid` naming the server and the key.
pub fn check(config: &Config) -> Result<(), Error> {
    for (name, server) in &config.mcp.servers {
        if !is_server_name(name) {
            return Err(invalid(format!(
                "mcp.servers.{name}: a server name is 1-64 letters, digits, '_' or '-'"
            )));
        }
        check_server(&format!("mcp.servers.{name}"), server)?;
    }
    Ok(())
}

fn check_server(key: &str, server: &McpServerConfig) -> Result<(), Error> {
    match server.transport {
        McpTransport::Stdio if server.command.trim().is_empty() => {
            return Err(invalid(format!(
                "{key}.command: a stdio server needs a command"
            )));
        }
        McpTransport::Http if !is_http_url(&server.url) => {
            return Err(invalid(format!(
                "{key}.url: an http server needs an http:// or https:// URL"
            )));
        }
        _ => {}
    }
    check_http_only(key, server)?;
    let named = [
        ("auth_secret", server.auth_secret.as_str()),
        (
            "client_key_password_secret",
            server.client_key_password_secret.as_str(),
        ),
        (
            "oauth.client_secret_name",
            server.oauth.client_secret_name.as_str(),
        ),
    ];
    let secrets = server
        .env_secrets
        .iter()
        .map(|s| ("env_secrets", s.as_str()))
        .chain(named.into_iter().filter(|(_, name)| !name.is_empty()));
    for (field, name) in secrets {
        crate::secrets::check_name(name)
            .map_err(|e| invalid(format!("{key}.{field}: {}", e.message())))?;
    }
    for host in &server.capabilities.network {
        if host.trim().is_empty() || host == "*" {
            return Err(invalid(format!(
                "{key}.capabilities.network: list the hosts ('{host}' is not one; `*.example.com` is)"
            )));
        }
    }
    let paths = server
        .capabilities
        .fs_read
        .iter()
        .map(|p| ("fs_read", p))
        .chain(server.capabilities.fs_write.iter().map(|p| ("fs_write", p)));
    for (field, path) in paths {
        if !path.starts_with("$workspace") && !std::path::Path::new(path).is_absolute() {
            return Err(invalid(format!(
                "{key}.capabilities.{field}: '{path}' must be absolute or start with $workspace"
            )));
        }
    }
    let globs = server
        .tools
        .include
        .iter()
        .flatten()
        .map(|g| ("include", g))
        .chain(server.tools.exclude.iter().map(|g| ("exclude", g)));
    for (field, pattern) in globs {
        glob::Pattern::new(pattern)
            .map_err(|e| invalid(format!("{key}.tools.{field}: '{pattern}': {e}")))?;
    }
    Ok(())
}

/// OAuth and client certificates belong to `http` servers; a spend limit
/// is a number of 0 or more.
fn check_http_only(key: &str, server: &McpServerConfig) -> Result<(), Error> {
    let stdio = server.transport == McpTransport::Stdio;
    if stdio && server.auth == Some(crate::schema::McpAuth::Oauth) {
        return Err(invalid(format!(
            "{key}.auth: OAuth applies to http servers only"
        )));
    }
    if stdio && !server.client_cert.is_empty() {
        return Err(invalid(format!(
            "{key}.client_cert: client certificates apply to http servers only"
        )));
    }
    let redirect = &server.oauth.redirect_uri;
    if !redirect.is_empty() && !is_http_url(redirect) {
        return Err(invalid(format!(
            "{key}.oauth.redirect_uri: expected an http:// or https:// URL"
        )));
    }
    if !server.spend_limit.is_finite() || server.spend_limit < 0.0 {
        return Err(invalid(format!(
            "{key}.spend_limit: expected 0 (no limit) or more"
        )));
    }
    Ok(())
}

fn is_http_url(url: &str) -> bool {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .is_some_and(|rest| !rest.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::McpServerConfig;

    fn with(name: &str, server: McpServerConfig) -> Config {
        let mut config = Config::default();
        config.mcp.servers.insert(name.to_owned(), server);
        config
    }

    #[test]
    fn a_stdio_server_without_a_command_is_refused() {
        let err = check(&with("gh", McpServerConfig::default())).unwrap_err();
        assert_eq!(
            err.message(),
            "mcp.servers.gh.command: a stdio server needs a command"
        );
    }

    #[test]
    fn names_urls_paths_and_globs_are_checked() {
        let stdio = McpServerConfig {
            command: "srv".into(),
            ..McpServerConfig::default()
        };
        assert!(check(&with("git-hub_2", stdio.clone())).is_ok());
        assert!(check(&with("a.b", stdio.clone())).is_err());
        let http = McpServerConfig {
            transport: McpTransport::Http,
            url: "ftp://x".into(),
            ..McpServerConfig::default()
        };
        assert!(check(&with("h", http)).is_err());
        let mut relative = stdio.clone();
        relative.capabilities.fs_read = vec!["data".into()];
        assert!(check(&with("s", relative)).is_err());
        let mut glob = stdio.clone();
        glob.tools.exclude = vec!["[".into()];
        assert!(check(&with("s", glob)).is_err());
        let mut secret = stdio.clone();
        secret.env_secrets = vec!["lower".into()];
        assert!(check(&with("s", secret)).is_err());
        let mut oauth = stdio;
        oauth.auth = Some(crate::schema::McpAuth::Oauth);
        let err = check(&with("s", oauth)).unwrap_err();
        assert_eq!(
            err.message(),
            "mcp.servers.s.auth: OAuth applies to http servers only"
        );
    }
}
