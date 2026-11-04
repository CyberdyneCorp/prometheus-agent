//! A sidecar's confined command (spec: python-bridge, Capability grants
//! enforced at the process boundary; security-boundaries, Egress proxy for
//! sandboxed children).
//!
//! The OS sandbox lets the sidecar read the system toolchain, its plugin
//! directory, its environment, the managed interpreter and the shim, and
//! write only its data directory and private temporary directory, plus the
//! `fs_read`/`fs_write` paths it was granted. With a `network` grant it may
//! connect only to its own loopback egress proxy, which admits the granted
//! hosts; without one it has no network. It starts children only with
//! `subprocess` granted. Each start gets a fresh proxy, kept until the
//! next start.

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError};

use pmt_config::ProfileScope;
use pmt_core::capability::{Capability, CapabilitySet};
use pmt_core::error::Error;
use pmt_net::DestinationPolicy;
use pmt_net::destination::PrivateGrant;
use pmt_net::egress::{Allowed, Proxy, Rules};
use pmt_plugins::grants::PluginGrants;
use pmt_pybridge::launch::{self, LaunchEnv};
use pmt_pybridge::supervisor::Launcher;
use pmt_sandbox::{Paths, Policy, Sandbox};

/// Where one plugin's sidecar runs from.
pub(super) struct Layout {
    /// The plugin directory.
    pub plugin_dir: PathBuf,
    /// Its environment.
    pub env: pmt_pybridge::env::Env,
    /// The managed interpreter's install directory.
    pub python_dir: PathBuf,
    /// The shim.
    pub shim: PathBuf,
    /// `<profile>/plugin-data/<id>`.
    pub data: PathBuf,
    /// Its private temporary directory.
    pub tmp: PathBuf,
}

/// The sandboxed launcher of one plugin.
pub(super) struct Sandboxed {
    pub scope: ProfileScope,
    pub layout: Layout,
    pub grants: Arc<PluginGrants>,
    pub proxy: Mutex<Option<Proxy>>,
}

fn canonical(path: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

impl Sandboxed {
    /// The capabilities the sidecar runs with; `proxy_port` when networked.
    fn capabilities(
        &self,
        granted: &CapabilitySet,
        proxy_port: Option<u16>,
    ) -> Result<CapabilitySet, Error> {
        let layout = &self.layout;
        let mut texts = vec![
            "fs.read:<toolchain>".to_owned(),
            "fs.read:<scratch>/**".to_owned(),
            "fs.write:<scratch>/**".to_owned(),
        ];
        for dir in [
            &layout.plugin_dir,
            &layout.env.dir,
            &layout.python_dir,
            &layout.shim,
        ] {
            // The environment names the interpreter by the path it was
            // created with, which may go through a symlink.
            texts.push(format!("fs.read:{}/**", canonical(dir).display()));
            texts.push(format!("fs.read:{}/**", dir.display()));
        }
        if let Some(port) = proxy_port {
            texts.push(format!("net.connect:*:{port}"));
        }
        let mut set: CapabilitySet = texts
            .iter()
            .map(|t| Capability::parse(t))
            .collect::<Result<_, _>>()?;
        for capability in granted {
            if matches!(capability, Capability::FsRead(_) | Capability::FsWrite(_)) {
                set = set.union(&std::iter::once(capability.clone()).collect());
            }
        }
        if granted.allows(&Capability::ProcessSpawn("*".to_owned())) {
            set = set.union(&std::iter::once(Capability::ProcessSpawn("*".to_owned())).collect());
        }
        Ok(set)
    }

    /// An egress proxy for the granted hosts, when there are any.
    fn proxy(&self, granted: &CapabilitySet) -> Result<Option<Proxy>, Error> {
        let mut allowed = Vec::new();
        for capability in granted {
            if let Capability::NetConnect { host, port } = capability {
                allowed.push(Allowed {
                    host: host.clone(),
                    port: *port,
                });
            }
        }
        if allowed.is_empty() {
            return Ok(None);
        }
        let egress = &self.scope.config().security.egress;
        let mut policy = DestinationPolicy::new(egress.allow_private, &egress.fake_ip_ranges)?;
        for entry in &allowed {
            let text = entry
                .port
                .map_or_else(|| entry.host.clone(), |p| format!("{}:{p}", entry.host));
            if let Ok(grant) = PrivateGrant::parse(&text) {
                policy = policy.with_grant(grant);
            }
        }
        Proxy::start(Rules { allowed, policy })
            .map(Some)
            .map_err(|e| {
                Error::new(
                    &pmt_core::error::codes::BRIDGE_CALL,
                    format!("egress proxy did not start: {e}"),
                )
            })
    }

    fn secrets(&self, granted: &CapabilitySet) -> Vec<(String, String)> {
        granted
            .iter()
            .filter_map(|c| match c {
                Capability::SecretsRead(name) => {
                    let value = self.scope.secrets().resolve(name).ok()?;
                    Some((name.clone(), value.expose().to_owned()))
                }
                _ => None,
            })
            .collect()
    }
}

impl Launcher for Sandboxed {
    fn command(&self) -> Result<Command, Error> {
        let granted = self.grants.current();
        let proxy = self.proxy(&granted)?;
        let capabilities = self.capabilities(&granted, proxy.as_ref().map(|p| p.addr().port()))?;
        let layout = &self.layout;
        for dir in [&layout.data, &layout.tmp] {
            std::fs::create_dir_all(dir)
                .map_err(|e| Error::io(&pmt_core::error::codes::BRIDGE_CALL, dir, &e))?;
        }
        let paths = Paths {
            user_home: std::env::var_os("HOME").map(Into::into).unwrap_or_default(),
            prometheus_home: self.scope.home().to_path_buf(),
            workspace: layout.data.clone(),
            scratch: vec![canonical(&layout.data), canonical(&layout.tmp)],
        };
        let sandbox = Sandbox::new(
            Policy::from_set(&capabilities, &paths),
            &self.scope.config().security.sandbox,
        )?;
        let mut command = sandbox.command(&layout.env.python)?;
        command.current_dir(&layout.data);
        let env = LaunchEnv {
            home: layout.data.clone(),
            tmp: layout.tmp.clone(),
            path: vec![layout.env.bin.clone()],
            secrets: self.secrets(&granted),
            proxy: proxy.as_ref().map(Proxy::url),
        };
        launch::configure(&mut command, &layout.shim, &env);
        *self.proxy.lock().unwrap_or_else(PoisonError::into_inner) = proxy;
        Ok(command)
    }
}
