//! The OS sandbox a plugin's sidecar runs in (spec: platform-adapters,
//! Supervised sidecar adapters; Raft wake-only adapter: the bridge child
//! runs sandboxed). A sidecar is code the plugin ships, so it gets what
//! the plugin was granted and nothing of the daemon's:
//!
//! - read: the system toolchain, the plugin directory, the runtime that
//!   runs it (the managed Node and the plugin's installed packages), the
//!   plugin's granted `fs.read` paths and, when `[sidecar] reads_home`,
//!   the user's home (a CLI it starts finds its own login there);
//!   credential stores stay denied either way;
//! - write: its own data directory (`<profile>/plugin-data/<id>/sidecar`,
//!   also its `TMPDIR`) and the plugin's granted `fs.write` paths;
//! - network: listening on its loopback port, and connecting on the
//!   ports of the plugin's `net.connect` grants. The OS filters ports,
//!   not hosts, so the hosts of those grants are held by the sidecar
//!   itself (the socket bridge refuses any other host, `PMT_SIDECAR_NETWORK`);
//! - processes: only with the plugin's granted `process.spawn`.

use std::path::{Path, PathBuf};
use std::process::Command;

use pmt_config::schema::SandboxConfig;
use pmt_core::capability::{Capability, CapabilitySet};
use pmt_sandbox::{Paths, Policy, Sandbox};

/// What confines one plugin's sidecar.
#[derive(Debug, Clone)]
pub(crate) struct Confinement {
    /// The profile's `security.sandbox`.
    pub sandbox: SandboxConfig,
    /// `PROMETHEUS_HOME` (denied, but for the paths granted inside it).
    pub home: PathBuf,
    /// The plugin's live grants.
    pub grants: CapabilitySet,
    /// Its writable data directory.
    pub data: PathBuf,
    /// Its children may read the user's home.
    pub reads_home: bool,
}

/// The parts of the plugin's grants a sidecar keeps: files, processes,
/// and its connections reduced to ports (what the OS can enforce).
fn kept(grants: &CapabilitySet) -> Vec<Capability> {
    grants
        .iter()
        .filter_map(|capability| match capability {
            Capability::FsRead(_) | Capability::FsWrite(_) | Capability::ProcessSpawn(_) => {
                Some(capability.clone())
            }
            Capability::NetConnect { port, .. } => Some(Capability::NetConnect {
                host: "*".to_owned(),
                port: *port,
            }),
            _ => None,
        })
        .collect()
}

fn read_tree(dir: &Path) -> Capability {
    Capability::FsRead(format!("{}/**", dir.display()))
}

/// The capabilities of a sidecar serving `port`, run by `program` from
/// `plugin_dir` (with `extra_reads`: its runtime and packages).
pub(crate) fn capabilities(
    confinement: &Confinement,
    plugin_dir: &Path,
    extra_reads: &[PathBuf],
    port: u16,
) -> CapabilitySet {
    let mut all = vec![
        Capability::FsRead("<toolchain>".to_owned()),
        Capability::FsRead("<workspace>/**".to_owned()),
        Capability::FsWrite("<workspace>/**".to_owned()),
        Capability::NetListen { port: Some(port) },
        read_tree(plugin_dir),
    ];
    all.extend(extra_reads.iter().map(|dir| read_tree(dir)));
    if confinement.reads_home {
        all.push(Capability::FsRead("~/**".to_owned()));
    }
    all.extend(kept(&confinement.grants));
    all.into_iter().collect()
}

/// A command for `program` confined as above.
///
/// # Errors
///
/// The data directory cannot be made, or the sandbox refuses to run (a
/// hardening feature missing that `security.sandbox.allow_degraded` does
/// not allow) or cannot be built.
pub(crate) fn command(
    confinement: &Confinement,
    program: &Path,
    plugin_dir: &Path,
    extra_reads: &[PathBuf],
    port: u16,
) -> Result<Command, String> {
    std::fs::create_dir_all(&confinement.data)
        .map_err(|e| format!("{}: {e}", confinement.data.display()))?;
    let data = std::fs::canonicalize(&confinement.data).unwrap_or(confinement.data.clone());
    let paths = Paths {
        user_home: std::env::var_os("HOME").map(Into::into).unwrap_or_default(),
        prometheus_home: confinement.home.clone(),
        workspace: data.clone(),
        scratch: vec![data],
    };
    let capabilities = capabilities(confinement, plugin_dir, extra_reads, port);
    let policy = Policy::from_set(&capabilities, &paths);
    let sandbox = Sandbox::new(policy, &confinement.sandbox).map_err(|e| e.message().to_owned())?;
    sandbox.command(program).map_err(|e| e.message().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn confinement(grants: &[&str], reads_home: bool) -> Confinement {
        Confinement {
            sandbox: SandboxConfig::default(),
            home: PathBuf::from("/home"),
            grants: CapabilitySet::parse(grants).unwrap(),
            data: PathBuf::from("/data"),
            reads_home,
        }
    }

    #[test]
    fn a_sidecar_keeps_its_plugin_grants_with_hosts_reduced_to_ports() {
        let set = capabilities(
            &confinement(
                &[
                    "net.connect:api.example.com:443",
                    "secrets.read:TOKEN",
                    "fs.read:/srv/files/**",
                    "llm.call",
                ],
                false,
            ),
            Path::new("/plugins/x"),
            &[PathBuf::from("/runtimes/node")],
            4100,
        );
        let texts: Vec<String> = set.iter().map(ToString::to_string).collect();
        for wanted in [
            "net.connect:*:443",
            "net.listen:loopback:4100",
            "fs.read:/srv/files/**",
            "fs.read:/plugins/x/**",
            "fs.read:/runtimes/node/**",
        ] {
            assert!(texts.iter().any(|t| t == wanted), "{wanted} in {texts:?}");
        }
        // Secrets travel by environment, models are not the sidecar's,
        // and nothing spawns or reads the home without being granted.
        for absent in ["secrets.read", "llm.call", "process.spawn", "fs.read:~"] {
            assert!(
                !texts.iter().any(|t| t.starts_with(absent)),
                "{absent} in {texts:?}"
            );
        }
        let home = capabilities(
            &confinement(&["process.spawn:*"], true),
            Path::new("/plugins/raft"),
            &[],
            1,
        );
        let texts: Vec<String> = home.iter().map(ToString::to_string).collect();
        assert!(texts.iter().any(|t| t == "fs.read:~/**"), "{texts:?}");
        assert!(texts.iter().any(|t| t == "process.spawn:*"), "{texts:?}");
    }
}
