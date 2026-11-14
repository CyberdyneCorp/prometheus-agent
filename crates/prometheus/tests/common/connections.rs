//! The desktop shell over remote daemons (spec: desktop-app, Connection
//! registry and what follows it): a shell with its own data directory and
//! an in-memory keychain, a WebSocket dial of what `prepare_dial` hands
//! the page, and stand-ins for `ssh` and `ssh-keyscan` that run the remote
//! command on this machine against another home.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_config::secrets::keychain::MemoryKeychain;
use pmt_desktop::connections::service::ConnectionsConfig;
use pmt_desktop::connections::ssh::SshPrograms;
use pmt_desktop::{Native, Request, Shell, ShellConfig, ensure};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;

/// A machine that opens nothing.
pub struct Quiet;

impl Native for Quiet {
    fn open_url(&self, _url: &str) -> Result<(), String> {
        Ok(())
    }
    fn open_path(&self, _path: &Path) -> Result<(), String> {
        Ok(())
    }
    fn reveal(&self, _path: &Path) -> Result<(), String> {
        Ok(())
    }
    fn pick(&self, _directory: bool, _title: Option<&str>) -> Option<PathBuf> {
        None
    }
    fn notify(&self, _title: &str, _body: &str) -> Result<(), String> {
        Ok(())
    }
    fn set_badge(&self, _window: &str, _count: Option<u32>) -> Result<(), String> {
        Ok(())
    }
    fn register_hotkey(&self, _chord: &str) -> Result<(), String> {
        Ok(())
    }
    fn read_clipboard(&self) -> Result<String, String> {
        Ok(String::new())
    }
}

/// `prometheus` on `home` as a script (the app's bundled binary, or the
/// binary on a remote host's `PATH`).
pub fn wrapper(home: &Path, at: &Path) {
    let user_home = super::user_home(home);
    let script = format!(
        "#!/bin/sh\nHOME='{}' exec '{}' --home '{}' \"$@\"\n",
        user_home.display(),
        super::BIN,
        home.display()
    );
    std::fs::write(at, script).unwrap();
    std::fs::set_permissions(at, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A shell whose local daemon is `local`'s, keeping its registry in
/// `app` with an in-memory keychain and `programs` as `ssh`.
pub fn shell(local: &Path, app: &Path, programs: SshPrograms) -> (Shell, MemoryKeychain) {
    let bundled = app.join("bundled-prometheus");
    std::fs::create_dir_all(app).unwrap();
    wrapper(local, &bundled);
    let keychain = MemoryKeychain::default();
    let shell = Shell::new(
        ShellConfig {
            binary: bundled,
            home: local.to_path_buf(),
            user_home: super::user_home(local),
            ensure_bound: ensure::DEADLINE + ensure::GRACE,
        },
        Arc::new(Quiet),
    )
    .with_connections(ConnectionsConfig {
        dir: app.join("data"),
        keychain: Arc::new(keychain.clone()),
        ssh: programs,
    });
    (shell, keychain)
}

/// One shell request as the page sends it.
pub async fn ask(shell: &Shell, command: &str, args: Value) -> Result<Value, String> {
    let request: Request =
        serde_json::from_value(json!({"command": command, "args": args})).unwrap();
    shell.handle("main", request).await
}

/// A request without arguments.
pub async fn ask0(shell: &Shell, command: &str) -> Result<Value, String> {
    let request: Request = serde_json::from_value(json!({"command": command})).unwrap();
    shell.handle("main", request).await
}

/// The socket type of a page dial.
pub type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Opens what `prepare_dial` answered, as the webview does (its origin
/// included) and says `rpc.hello`.
pub async fn dial(answer: &Value) -> Result<Socket, String> {
    let url = answer["url"].as_str().unwrap();
    let protocols: Vec<&str> = answer["protocols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let mut request = url.into_client_request().unwrap();
    let headers = request.headers_mut();
    headers.insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_str(&protocols.join(", ")).unwrap(),
    );
    headers.insert("Origin", HeaderValue::from_static("tauri://localhost"));
    let (mut ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| e.to_string())?;
    let hello = super::ws::request_on(&mut ws, &hello()).await;
    if hello.get("result").is_none() {
        return Err(hello.to_string());
    }
    Ok(ws)
}

/// The desktop's `rpc.hello`.
pub fn hello() -> Value {
    json!({"jsonrpc": "2.0", "id": 0, "method": "rpc.hello", "params": {
        "client": "desktop", "client_version": "test", "protocol": {"major": 1, "minor": 0},
        "capabilities": {"server_requests": true, "events": []}}})
}

/// One call on a dialed socket; the whole answer.
pub async fn call(ws: &mut Socket, id: u64, method: &str, params: Value) -> Value {
    let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    super::ws::request_on(ws, &frame).await
}

/// Closes a socket.
pub async fn close(mut ws: Socket) {
    let _ = ws.close(None).await;
}

/// Stand-ins for `ssh` and `ssh-keyscan` in `dir`: the host presents the
/// key in `dir/hostkey`; a known host runs the remote command with
/// `dir/hosts/<host>` as its whole `PATH` (plus the system's), so a host
/// without a `prometheus` there has none; `-W` pipes to the local port.
/// Every `ssh` run appends its arguments to `dir/calls`.
pub fn fake_ssh(dir: &Path) -> SshPrograms {
    std::fs::create_dir_all(dir.join("hosts")).unwrap();
    set_host_key(dir, HOST_KEY_A);
    let ssh = format!(
        r#"#!/bin/bash
dir='{dir}'
echo "$*" >> "$dir/calls"
known=""; forward=""; host=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) case "$2" in UserKnownHostsFile=*) known="${{2#UserKnownHostsFile=}}";; esac; shift 2;;
    -p|-l) shift 2;;
    -W) forward="$2"; shift 2;;
    --) shift; break;;
    *) host="$1"; shift;;
  esac
done
key="$(cat "$dir/hostkey")"
entry="$(grep "^$host " "$known" 2>/dev/null | head -1)"
if [ -z "$entry" ]; then
  echo "No ED25519 host key is known for $host and you have requested strict checking." >&2
  echo "Host key verification failed." >&2
  exit 255
fi
if [ "${{entry#* }}" != "$key" ]; then
  echo "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@" >&2
  echo "@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @" >&2
  echo "Host key verification failed." >&2
  exit 255
fi
if [ -n "$forward" ]; then
  exec 3<>"/dev/tcp/${{forward%:*}}/${{forward##*:}}"
  cat <&3 &
  reader=$!
  cat >&3
  wait $reader
  exit 0
fi
PATH="$dir/hosts/$host:/usr/bin:/bin" exec /bin/sh -c "$*"
"#,
        dir = dir.display()
    );
    let keyscan = format!(
        "#!/bin/sh\nfor a; do host=\"$a\"; done\necho \"$host $(cat '{}/hostkey')\"\n",
        dir.display()
    );
    let programs = SshPrograms {
        ssh: dir.join("ssh"),
        keyscan: dir.join("ssh-keyscan"),
    };
    for (path, text) in [(&programs.ssh, ssh), (&programs.keyscan, keyscan)] {
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    programs
}

/// Two Ed25519 host keys (public halves only).
pub const HOST_KEY_A: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
pub const HOST_KEY_B: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAJa5E4vU/vthhKAqlaCTgipkPBC9Jwc8e2PY/vmbLab";

/// The host now presents `key`.
pub fn set_host_key(dir: &Path, key: &str) {
    std::fs::write(dir.join("hostkey"), key).unwrap();
}

/// A remote host `name` whose `PATH` runs `prometheus` on `home`.
pub fn host_with_prometheus(dir: &Path, name: &str, home: &Path) {
    let bin = dir.join("hosts").join(name);
    std::fs::create_dir_all(&bin).unwrap();
    wrapper(home, &bin.join("prometheus"));
}

/// How many times `ssh` ran.
pub fn ssh_runs(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("calls")).map_or(0, |t| t.lines().count())
}
