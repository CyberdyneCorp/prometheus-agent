//! Supply-chain advisories against this install (spec:
//! security-boundaries, Supply-chain advisory catalog): the catalog in
//! force (`pmt_update::advisories`) matched at startup against installed
//! plugins, python-bridge environments, pinned MCP server packages and
//! managed runtimes. An unacknowledged match is one WARN line per start
//! (stderr on an interactive terminal, the profile's and the gateway's
//! logs); `prometheus doctor security` always shows the detail and
//! remediation; `prometheus doctor --ack <id>` silences the warning only.

use std::path::Path;

use pmt_config::ProfileScope;
use pmt_config::schema::McpServerConfig;
use pmt_core::error::{Error, codes};
use pmt_update::advisories::{Catalog, Ecosystem, Installed, Match};
use pmt_update::trust::TrustRoot;

/// How deep `<profile>/python/` is searched for `bridge.lock` files.
const LOCK_DEPTH: usize = 4;

/// The catalog in force for `scope`'s home.
#[must_use]
pub fn catalog(scope: &ProfileScope) -> Catalog {
    Catalog::current(scope.home(), &TrustRoot::embedded())
}

/// Every match of the catalog against `scope`'s installed components.
#[must_use]
pub fn matches(scope: &ProfileScope) -> Vec<Match> {
    catalog(scope).matches(&installed(scope))
}

/// Logs one WARN per match not in `security.acked_advisories`.
pub fn warn_unacked(scope: &ProfileScope) {
    let acked = &scope.config().security.acked_advisories;
    for found in matches(scope)
        .iter()
        .filter(|m| !acked.iter().any(|a| a.eq_ignore_ascii_case(&m.advisory.id)))
    {
        tracing::warn!("{}", warning(found));
    }
}

/// The one-line startup warning of `found`.
#[must_use]
pub fn warning(found: &Match) -> String {
    format!(
        "advisory {}: {} {} ({}) is a known-compromised version; see prometheus doctor security, silence with prometheus doctor --ack {}",
        found.advisory.id,
        found.installed.name,
        found.installed.version,
        found.installed.location,
        found.advisory.id
    )
}

/// `prometheus doctor --ack <id>`: appends `id` to
/// `security.acked_advisories`.
///
/// # Errors
///
/// `advisory.unknown` (66) for an id the catalog does not have; config
/// write errors.
pub fn ack(scope: &ProfileScope, id: &str) -> Result<String, Error> {
    let catalog = catalog(scope);
    let Some(advisory) = catalog.get(id) else {
        return Err(
            Error::new(&codes::ADVISORY_UNKNOWN, format!("unknown advisory '{id}'")).with_hint(
                "prometheus doctor security lists the advisories that match this install",
            ),
        );
    };
    let mut acked = scope.config().security.acked_advisories.clone();
    if !acked.iter().any(|a| a.eq_ignore_ascii_case(&advisory.id)) {
        acked.push(advisory.id.clone());
    }
    let quoted: Vec<String> = acked.iter().map(|s| format!("{s:?}")).collect();
    scope.profile().config_file().set_all(&[(
        "security.acked_advisories",
        &format!("[{}]", quoted.join(", ")),
    )])?;
    Ok(advisory.id.clone())
}

/// What this install has that advisories can name.
#[must_use]
pub fn installed(scope: &ProfileScope) -> Vec<Installed> {
    let mut found = plugins(scope.dir());
    found.extend(bridge_locks(&scope.dir().join("python")));
    for (name, server) in &scope.config().mcp.servers {
        found.extend(mcp_package(name, server));
    }
    found.extend(runtimes(scope.home()));
    found
}

/// Installed plugins: `<profile>/plugins/<id>/plugin.toml`.
fn plugins(profile: &Path) -> Vec<Installed> {
    let Ok(entries) = std::fs::read_dir(profile.join("plugins")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let text = std::fs::read_to_string(entry.path().join("plugin.toml")).ok()?;
            let manifest: toml::Table = toml::from_str(&text).ok()?;
            let id = manifest.get("id")?.as_str()?.to_owned();
            let version = manifest.get("version")?.as_str()?.to_owned();
            Some(Installed {
                ecosystem: Ecosystem::Plugin,
                location: format!("plugin {id}"),
                name: id,
                version,
            })
        })
        .collect()
}

/// Pinned packages of every `bridge.lock` under `dir` (`name==version`
/// lines of uv's resolution).
fn bridge_locks(dir: &Path) -> Vec<Installed> {
    let mut found = Vec::new();
    let mut pending = vec![(dir.to_path_buf(), 0)];
    while let Some((dir, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for path in entries.flatten().map(|e| e.path()) {
            if path.is_dir() && depth < LOCK_DEPTH && !path.is_symlink() {
                pending.push((path, depth + 1));
            } else if path
                .file_name()
                .is_some_and(|n| n == pmt_pybridge::env::LOCK_FILE)
            {
                let location = format!("python environment {}", dir.display());
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                found.extend(lock_lines(&text, &location));
            }
        }
    }
    found
}

fn lock_lines(text: &str, location: &str) -> Vec<Installed> {
    text.lines()
        .filter_map(|line| {
            let pin = line.split([' ', '\\', ';']).next()?.trim();
            let (name, version) = pin.split_once("==")?;
            let name = name.split('[').next()?.trim();
            (!name.is_empty() && !version.is_empty()).then(|| Installed {
                ecosystem: Ecosystem::Pypi,
                name: name.to_owned(),
                version: version.trim().to_owned(),
                location: location.to_owned(),
            })
        })
        .collect()
}

/// The pinned package an MCP server runs: `npx [-y] pkg@1.2.3`,
/// `bunx pkg@1.2.3`, `uvx pkg==1.2.3`, `uvx --from pkg==1.2.3 cmd`.
fn mcp_package(name: &str, server: &McpServerConfig) -> Option<Installed> {
    let program = Path::new(&server.command).file_stem()?.to_str()?;
    let location = format!("mcp server {name}");
    match program {
        "npx" | "bunx" => {
            let spec = server.args.iter().find(|a| !a.starts_with('-'))?;
            let (package, version) = npm_spec(spec)?;
            Some(Installed {
                ecosystem: Ecosystem::Npm,
                name: package.to_owned(),
                version: version.to_owned(),
                location,
            })
        }
        "uvx" => {
            let from = server
                .args
                .iter()
                .position(|a| a == "--from")
                .and_then(|i| server.args.get(i + 1));
            let spec = from.or_else(|| server.args.iter().find(|a| !a.starts_with('-')))?;
            let (package, version) = spec.split_once("==").or_else(|| spec.split_once('@'))?;
            Some(Installed {
                ecosystem: Ecosystem::Pypi,
                name: package.to_owned(),
                version: version.to_owned(),
                location,
            })
        }
        _ => None,
    }
}

/// `name@version` or `@scope/name@version`.
fn npm_spec(spec: &str) -> Option<(&str, &str)> {
    let at = spec.rfind('@').filter(|&i| i > 0)?;
    let (package, version) = (&spec[..at], &spec[at + 1..]);
    (!version.is_empty()).then_some((package, version))
}

/// Installed managed runtimes of the home.
fn runtimes(home: &Path) -> Vec<Installed> {
    let store = pmt_update::runtimes::Store::open(home);
    store
        .index()
        .runtimes
        .keys()
        .flat_map(|name| store.installed_versions(name))
        .map(|installed| Installed {
            ecosystem: Ecosystem::Runtime,
            location: format!("runtime {}", installed.record.name),
            name: installed.record.name,
            version: installed.record.version,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use pmt_config::schema::McpServerConfig;
    use pmt_update::advisories::Ecosystem;

    use super::{lock_lines, mcp_package};

    fn server(command: &str, args: &[&str]) -> McpServerConfig {
        McpServerConfig {
            command: command.into(),
            args: args.iter().map(|&a| a.to_owned()).collect(),
            ..McpServerConfig::default()
        }
    }

    #[test]
    fn pinned_mcp_packages_are_read_from_their_command() {
        let npx = mcp_package("s", &server("npx", &["-y", "@solana/web3.js@1.95.6"])).unwrap();
        assert_eq!(
            (npx.ecosystem, npx.name.as_str(), npx.version.as_str()),
            (Ecosystem::Npm, "@solana/web3.js", "1.95.6")
        );
        let uvx = mcp_package(
            "s",
            &server("/usr/bin/uvx", &["--from", "ultralytics==8.3.41", "yolo"]),
        )
        .unwrap();
        assert_eq!(
            (uvx.name.as_str(), uvx.version.as_str()),
            ("ultralytics", "8.3.41")
        );
        assert!(mcp_package("s", &server("npx", &["-y", "unpinned-server"])).is_none());
        assert!(mcp_package("s", &server("node", &["server.js"])).is_none());
    }

    #[test]
    fn bridge_lock_pins_are_read() {
        let lock = "requests==2.32.3 \\\n    --hash=sha256:abc\nultralytics[export]==8.3.41 ; python_version >= \"3.9\"\n# comment\n";
        let found = lock_lines(lock, "env");
        let pins: Vec<(&str, &str)> = found
            .iter()
            .map(|i| (i.name.as_str(), i.version.as_str()))
            .collect();
        assert_eq!(pins, [("requests", "2.32.3"), ("ultralytics", "8.3.41")]);
    }
}
