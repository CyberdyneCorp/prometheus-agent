//! The egress-only network of a profile's containers (spec:
//! security-boundaries, Egress proxy coverage of containers and remote
//! sandboxes: "a network that only reaches the proxy").
//!
//! Session containers join `pmt-egress-<profile>-<home digest>`, an `--internal`
//! network: the runtime gives it no route off the network, so nothing in
//! it reaches the host or the internet directly. Its one other member is
//! the relay container `pmt-egress-relay-<profile>-<home digest>`, also attached to the
//! runtime's default network, which forwards every connection to its
//! listener (alias [`RELAY_ALIAS`]) to the host's egress proxy and to
//! nothing else. The relay runs the session image (a `socat`, `python3`
//! or busybox `nc -e` it already has), without capabilities, read-only.

use pmt_core::error::{Error, codes};
use std::fmt::Write as _;

use sha2::Digest as _;

use super::{Flavor, Runtime, first_line};

/// The relay's name on the egress network: the proxy URL's host.
pub const RELAY_ALIAS: &str = "prometheus-egress";

/// The label naming the relay's owning profile (not the session
/// containers' profile label: orphan reaping never removes the relay).
pub const RELAY_LABEL: &str = "prometheus.egress-relay";

/// The label naming the home of the relay and the network.
pub const HOME_LABEL: &str = "prometheus.egress-home";

/// The label recording what the relay forwards to (a changed proxy port
/// or image replaces the relay).
const TARGET_LABEL: &str = "prometheus.egress-target";

/// Picks the first forwarder the image has; its arguments are the listen
/// port, the target host and the target port.
const RELAY_SCRIPT: &str = r#"set -eu
port="$1"; host="$2"; target="$3"
if command -v socat >/dev/null 2>&1; then
  exec socat "TCP-LISTEN:$port,fork,reuseaddr" "TCP:$host:$target"
fi
if command -v python3 >/dev/null 2>&1; then
  exec python3 -c '
import socket, sys, threading
port, host, target = int(sys.argv[1]), sys.argv[2], int(sys.argv[3])
def pipe(src, dst):
    try:
        while True:
            data = src.recv(65536)
            if not data:
                break
            dst.sendall(data)
    except OSError:
        pass
    for s in (src, dst):
        try:
            s.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
server = socket.create_server(("", port))
while True:
    client, _ = server.accept()
    try:
        upstream = socket.create_connection((host, target))
    except OSError:
        client.close()
        continue
    for a, b in ((client, upstream), (upstream, client)):
        threading.Thread(target=pipe, args=(a, b), daemon=True).start()
' "$port" "$host" "$target"
fi
if nc --help 2>&1 | grep -q -- '-e PROG'; then
  exec nc -lk -p "$port" -e nc "$host" "$target"
fi
echo "prometheus egress relay: the image has none of socat, python3 or a busybox nc with -e" >&2
exit 1
"#;

/// What a profile's egress network is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressNetwork {
    /// The internal network session containers join.
    pub network: String,
    /// The relay container.
    pub relay: String,
    /// The port the relay listens on (the proxy's bridge port).
    pub port: u16,
    /// The home the profile belongs to (a label of the relay).
    pub home: String,
}

impl EgressNetwork {
    /// The names for `profile` of the home `home_key` (two homes may both
    /// have a `default` profile), relaying to port `port`.
    #[must_use]
    pub fn of(profile: &str, home_key: &str, port: u16) -> Self {
        let clean: String = profile
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        // sha256 (not std's hasher): stable across releases.
        let digest = sha2::Sha256::digest(home_key.as_bytes());
        let home = digest[..4].iter().fold(String::new(), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        });
        Self {
            network: format!("pmt-egress-{clean}-{home}"),
            relay: format!("pmt-egress-relay-{clean}-{home}"),
            port,
            home: home_key.to_owned(),
        }
    }

    /// Removes the relay (the proxy stopped: nothing is left to relay
    /// to); the network stays for the containers attached to it.
    pub fn remove_relay(&self, runtime: &Runtime) {
        let _ = runtime.run(&["rm".into(), "-f".into(), self.relay.clone()]);
    }

    /// The proxy URL inside a session container.
    #[must_use]
    pub fn proxy_url(&self) -> String {
        format!("http://{RELAY_ALIAS}:{}", self.port)
    }

    /// Creates the network and (re)starts the relay when missing, stopped
    /// or made for another target or image. The relay reaches the host's
    /// proxy at the runtime's host alias on `self.port`.
    ///
    /// # Errors
    ///
    /// `terminal.container_runtime` naming the runtime call that failed.
    pub fn ensure(&self, runtime: &Runtime, profile: &str, image: &str) -> Result<(), Error> {
        self.ensure_network(runtime, profile)?;
        let (alias, add_host) = match runtime.flavor() {
            Flavor::Podman => ("host.containers.internal", None),
            Flavor::Docker => (
                "host.docker.internal",
                Some("--add-host=host.docker.internal:host-gateway".to_owned()),
            ),
        };
        let target = format!("{alias}:{}|{image}", self.port);
        if self.relay_current(runtime, &target)? {
            return Ok(());
        }
        // A stale relay (stopped, other target): replaced.
        let _ = runtime.run(&["rm".into(), "-f".into(), self.relay.clone()]);
        let mut args = vec![
            "run".to_owned(),
            "-d".to_owned(),
            format!("--name={}", self.relay),
            format!("--label={RELAY_LABEL}={profile}"),
            format!("--label={TARGET_LABEL}={target}"),
            format!("--label={HOME_LABEL}={}", self.home),
            "--cap-drop=ALL".to_owned(),
            "--security-opt=no-new-privileges".to_owned(),
            "--read-only".to_owned(),
            "--user=65534:65534".to_owned(),
            "--entrypoint=sh".to_owned(),
        ];
        args.extend(add_host);
        args.extend([
            image.to_owned(),
            "-c".to_owned(),
            RELAY_SCRIPT.to_owned(),
            "sh".to_owned(),
            self.port.to_string(),
            alias.to_owned(),
            self.port.to_string(),
        ]);
        checked(runtime, &args)?;
        checked(
            runtime,
            &[
                "network".into(),
                "connect".into(),
                format!("--alias={RELAY_ALIAS}"),
                self.network.clone(),
                self.relay.clone(),
            ],
        )?;
        Ok(())
    }

    fn ensure_network(&self, runtime: &Runtime, profile: &str) -> Result<(), Error> {
        let exists = runtime
            .run(&["network".into(), "inspect".into(), self.network.clone()])?
            .status
            .success();
        if exists {
            return Ok(());
        }
        let created = runtime.run(&[
            "network".into(),
            "create".into(),
            "--internal".into(),
            format!("--label={RELAY_LABEL}={profile}"),
            format!("--label={HOME_LABEL}={}", self.home),
            self.network.clone(),
        ])?;
        // A concurrent session may have created it meanwhile.
        if created.status.success()
            || runtime
                .run(&["network".into(), "inspect".into(), self.network.clone()])?
                .status
                .success()
        {
            return Ok(());
        }
        Err(failed("network create", &created.stderr))
    }

    /// Whether the relay runs and forwards to `target`.
    fn relay_current(&self, runtime: &Runtime, target: &str) -> Result<bool, Error> {
        let out = runtime.run(&[
            "inspect".into(),
            "--type=container".into(),
            format!(
                "--format={{{{.State.Running}}}}|{{{{index .Config.Labels \"{TARGET_LABEL}\"}}}}"
            ),
            self.relay.clone(),
        ])?;
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(out.status.success() && text.trim() == format!("true|{target}"))
    }
}

fn checked(runtime: &Runtime, args: &[String]) -> Result<(), Error> {
    let out = runtime.run(args)?;
    if out.status.success() {
        return Ok(());
    }
    let what = args.iter().take(2).cloned().collect::<Vec<_>>().join(" ");
    Err(failed(&what, &out.stderr))
}

fn failed(what: &str, stderr: &[u8]) -> Error {
    Error::new(
        &codes::TERMINAL_CONTAINER_RUNTIME,
        format!(
            "the egress network could not be set up ({what}): {}",
            first_line(stderr)
        ),
    )
}
