//! `prometheus egress serve`: the egress proxy process (see the parent
//! module). It reads the credential values once per start or reload
//! (`security.egress.credential_source`: the secrets store, or Bitwarden
//! refetched every time), binds its listeners, records itself and answers
//! `status`, `reload` and `stop` on its control port.

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_config::schema::CredentialSource;
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use pmt_net::egress::inject::{Authority, Injection, Mapping};
use pmt_net::egress::{Allowed, Proxy, Rules};
use serde_json::{Value, json};

use super::Record;

/// The running proxy's listeners.
struct Served {
    loopback: Proxy,
    bridge: Proxy,
    placeholders: BTreeMap<String, String>,
}

/// Runs the proxy of `scope` until `stop` (or, `owned`, until stdin
/// closes).
///
/// # Errors
///
/// `egress.proxy_start`: missing credentials, a listener that does not
/// bind, a proxy already recorded and answering.
pub fn serve(scope: &ProfileScope, owned: bool) -> Result<u8, Error> {
    let rules = rules(scope)?;
    let config = scope.config();
    let loopback = Proxy::start(rules.0.clone()).map_err(|e| bind_error("loopback", &e))?;
    let bridge_addr = SocketAddr::new(bridge_ip(scope), config.security.egress.proxy_port);
    let bridge = Proxy::start_at(rules.0, bridge_addr)
        .map_err(|e| bind_error(&bridge_addr.to_string(), &e))?;
    let control = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| bind_error("control", &e))?;
    let record = Record {
        pid: std::process::id(),
        nonce: pmt_net::egress::inject::placeholder(),
        started_at: jiff::Timestamp::now().to_string(),
        control: control
            .local_addr()
            .map_err(|e| bind_error("control", &e))?,
        loopback: loopback.addr(),
        bridge: bridge.addr(),
    };
    write_record(scope, &record)?;
    if owned {
        // The daemon holds our stdin: its end is our end.
        std::thread::spawn(|| {
            let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
            std::process::exit(0);
        });
    }
    let mut served = Served {
        loopback,
        bridge,
        placeholders: rules.1,
    };
    for stream in control.incoming().flatten() {
        if answer(scope, &record, &mut served, &stream) {
            break;
        }
    }
    let _ = std::fs::remove_file(super::record_path(scope));
    Ok(0)
}

/// Handles one control request; `true` to stop.
fn answer(scope: &ProfileScope, record: &Record, served: &mut Served, stream: &TcpStream) -> bool {
    let mut line = String::new();
    let Ok(clone) = stream.try_clone() else {
        return false;
    };
    let _ = BufReader::new(clone.take(4096)).read_line(&mut line);
    let request: Value = serde_json::from_str(&line).unwrap_or_default();
    let reply = |value: Value| {
        let mut stream = stream;
        let _ = stream.write_all(format!("{value}\n").as_bytes());
    };
    if request["nonce"].as_str() != Some(record.nonce.as_str()) {
        reply(json!({"error": "wrong nonce"}));
        return false;
    }
    match request["op"].as_str().unwrap_or_default() {
        "status" => reply(status(scope, record, served)),
        "reload" => match rules(scope) {
            Ok((rules, placeholders)) => {
                served.loopback.reload(rules.clone());
                served.bridge.reload(rules);
                served.placeholders = placeholders;
                reply(json!({"reloaded": true}));
            }
            Err(err) => reply(json!({"error": err.message()})),
        },
        "stop" => {
            reply(json!({"stopping": true}));
            return true;
        }
        other => reply(json!({"error": format!("unknown op '{other}'")})),
    }
    false
}

fn status(scope: &ProfileScope, record: &Record, served: &Served) -> Value {
    json!({
        "pid": record.pid,
        "started_at": record.started_at,
        "mode": scope.config().security.egress.mode.as_str(),
        "loopback": served.loopback.addr().to_string(),
        "bridge": served.bridge.addr().to_string(),
        "denied": served.loopback.denied() + served.bridge.denied(),
        "placeholders": served.placeholders,
    })
}

fn write_record(scope: &ProfileScope, record: &Record) -> Result<(), Error> {
    let path = super::record_path(scope);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(&codes::HOME_IO, dir, &e))?;
    }
    // A record nobody answers for is left over from a crash.
    if super::running(scope).is_some() {
        return Err(super::start_error(
            "a proxy of this profile is already running",
        ));
    }
    let _ = std::fs::remove_file(&path);
    // Created exclusively: never through a link someone placed there.
    let mut file = pmt_core::private_fs::new_file_options()
        .open(&path)
        .map_err(|e| Error::io(&codes::HOME_IO, &path, &e))?;
    file.write_all(&serde_json::to_vec(record).expect("a record serializes"))
        .map_err(|e| Error::io(&codes::HOME_IO, &path, &e))
}

fn bind_error(what: &str, e: &std::io::Error) -> Error {
    super::start_error(&format!("cannot listen on {what}: {e}"))
}

/// The container bridge address: the runtime's bridge gateway on Linux
/// (where containers reach the host), loopback elsewhere (Docker Desktop
/// and Podman machines forward the host alias to it).
fn bridge_ip(scope: &ProfileScope) -> std::net::IpAddr {
    let loopback = std::net::IpAddr::from([127, 0, 0, 1]);
    if !cfg!(target_os = "linux") {
        return loopback;
    }
    let setting = &scope.config().terminal.container.runtime;
    let Ok(runtime) = pmt_sandbox::container::Runtime::detect(setting) else {
        return loopback;
    };
    let (network, format) = match runtime.flavor() {
        pmt_sandbox::container::Flavor::Docker => ("bridge", "{{(index .IPAM.Config 0).Gateway}}"),
        pmt_sandbox::container::Flavor::Podman => ("podman", "{{(index .Subnets 0).Gateway}}"),
    };
    runtime
        .command()
        .args(["network", "inspect", network, "--format", format])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(loopback)
}

/// The proxy's rules and placeholders: the profile's host policy, private
/// and metadata addresses always refused upstream, and every mapping of
/// `security.egress.inject` with its value from the credential source.
fn rules(scope: &ProfileScope) -> Result<(Rules, BTreeMap<String, String>), Error> {
    let egress = &scope.config().security.egress;
    let placeholders = crate::egress::mappings::ensure(scope)?;
    let values = values(scope, placeholders.keys())?;
    let config = scope.config();
    let mappings = placeholders
        .iter()
        .filter_map(|(name, placeholder)| {
            let value = values.get(name)?.clone();
            let hosts = pmt_config::egress_check::inject_hosts(config, name)
                .iter()
                .map(|h| Allowed::parse(h))
                .collect();
            Some(Mapping {
                name: name.clone(),
                placeholder: placeholder.clone(),
                value,
                hosts,
            })
        })
        .collect();
    let dir = crate::egress::injection::ca_dir(scope);
    let authority = Authority::open(&dir)
        .map_err(|e| super::start_error(&format!("the egress CA in {}: {e}", dir.display())))?;
    let extra_ca = scope.config().network.extra_ca_file.clone();
    let mut upstream = pmt_net::tls::client_config(extra_ca.as_deref().map(std::path::Path::new))?;
    upstream.alpn_protocols = vec![b"http/1.1".to_vec()];
    let hosts = Arc::new(crate::egress::host_policy(
        egress,
        &crate::egress::Overrides::default(),
    ));
    // Upstream, private and metadata addresses are refused whatever the
    // allowlists say.
    let destinations = pmt_net::DestinationPolicy::new(false, &egress.fake_ip_ranges)?;
    let rules = Rules::new(hosts, destinations, "egress-proxy").injecting(Arc::new(Injection {
        mappings,
        authority: Arc::new(authority),
        upstream: Arc::new(upstream),
    }));
    Ok((rules, placeholders))
}

/// The mapped secrets' values from `security.egress.credential_source`.
fn values<'a>(
    scope: &ProfileScope,
    names: impl Iterator<Item = &'a String>,
) -> Result<BTreeMap<String, SecretString>, Error> {
    let names: Vec<&String> = names.collect();
    let egress = &scope.config().security.egress;
    let store = |name: &str| scope.secrets().resolve(name).ok();
    let mut found = BTreeMap::new();
    let mut missing = Vec::new();
    let fetched = match egress.credential_source {
        CredentialSource::Store => None,
        CredentialSource::Bitwarden => Some(bitwarden(scope)),
    };
    for name in names {
        let value = match &fetched {
            None => store(name),
            Some(Ok(bws)) => bws.get(name.as_str()).cloned(),
            Some(Err(_)) => None,
        };
        let value = value.or_else(|| egress.allow_store_fallback.then(|| store(name)).flatten());
        match value {
            Some(value) => {
                found.insert(name.clone(), value);
            }
            None => missing.push(name.as_str()),
        }
    }
    if missing.is_empty() {
        return Ok(found);
    }
    let why = match &fetched {
        Some(Err(why)) => format!("Bitwarden: {why}; "),
        _ => String::new(),
    };
    Err(super::start_error(&format!(
        "{why}missing mapped secret(s): {}",
        missing.join(", ")
    )))
}

/// Every secret of the configured Bitwarden project, fetched now.
fn bitwarden(scope: &ProfileScope) -> Result<BTreeMap<String, SecretString>, String> {
    let config = &scope.config().secrets.bitwarden;
    let token_name = &config.access_token_secret.0;
    let token = scope
        .secrets()
        .store()
        .get(token_name)
        .ok()
        .flatten()
        .ok_or_else(|| format!("no access token ({token_name} is not set)"))?;
    let project = config
        .project_id
        .as_deref()
        .ok_or_else(|| "no project id (secrets.bitwarden.project_id)".to_owned())?;
    let program = pmt_config::secrets::external::program(scope.home(), "bws");
    let mut args = vec!["secret", "list", project, "--output", "json"];
    if let Some(url) = &config.server_url {
        args.extend(["--server-url", url]);
    }
    let out = pmt_config::secrets::external::run(&program, &args, "BWS_ACCESS_TOKEN", &token)?;
    let listed: Vec<Value> =
        serde_json::from_slice(&out).map_err(|_| "bws did not print a JSON list".to_owned())?;
    let mut values = BTreeMap::new();
    for entry in &listed {
        if let (Some(key), Some(value)) = (entry["key"].as_str(), entry["value"].as_str()) {
            values
                .entry(key.to_owned())
                .or_insert_with(|| SecretString::new(value));
        }
    }
    Ok(values)
}
