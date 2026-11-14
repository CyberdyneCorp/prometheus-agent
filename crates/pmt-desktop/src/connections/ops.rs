//! What the shell does across connections: Test (the HTTP health endpoint
//! and a WebSocket handshake), the union agent roster (listed over each
//! connection's HTTP API or `daemon status`, never by opening sockets) and
//! Update all instances (each remote daemon through `daemon.update`, in
//! parallel, the app itself last).

use futures_util::future::join_all;
use serde::Serialize;
use serde_json::{Value, json};
use url::Url;

use super::roster::{self, Listed, Roster};
use super::service::Latch;
use super::ssh::SshFailure;
use super::tls::{self, DialFailure};
use super::{Connection, Kind};
use crate::shell::Shell;
use crate::update::Mechanism;

/// One probe of a Test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Step {
    /// Whether it passed.
    pub ok: bool,
    /// What it found.
    pub detail: String,
}

impl Step {
    fn pass(detail: impl Into<String>) -> Self {
        Self {
            ok: true,
            detail: detail.into(),
        }
    }

    fn fail(detail: impl Into<String>) -> Self {
        Self {
            ok: false,
            detail: detail.into(),
        }
    }
}

/// A key the user must compare before the connection is used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Confirm {
    /// `certificate` (a remote server's key) or `host_key` (SSH).
    pub kind: &'static str,
    /// What the server presented.
    pub fingerprint: String,
}

/// What Test found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TestReport {
    /// The health probe (`/api/health`, `daemon ensure` over SSH, the
    /// local daemon's status).
    pub health: Step,
    /// The client protocol handshake and `rpc.hello`.
    pub websocket: Step,
    /// A key to confirm first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm: Option<Confirm>,
    /// A latched error (the shell does not dial until the user acts).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latched: Option<Latch>,
}

impl TestReport {
    fn stopped(health: Step) -> Self {
        Self {
            health,
            websocket: Step::fail("not tried"),
            confirm: None,
            latched: None,
        }
    }
}

/// One instance's row of Update all instances.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateRow {
    /// The connection.
    pub connection_id: String,
    /// Its name.
    pub name: String,
    /// `started`, `staged`, `up_to_date`, `refused`, `unreachable`,
    /// `unavailable` or `failed`.
    pub outcome: String,
    /// The details (an owner's refusal names the command that updates it).
    pub message: String,
}

impl Shell {
    /// Test: the health endpoint and a WebSocket handshake of `id`.
    pub(crate) async fn test_connection(&self, id: &str) -> Result<TestReport, String> {
        let connection = self.connections.get(id)?;
        if let Some(latch) = self.connections.shared.latched(id) {
            let mut report = TestReport::stopped(Step::fail(latch.message()));
            report.latched = Some(latch);
            return Ok(report);
        }
        let mut report = match connection.kind {
            Kind::Local => self.test_local().await,
            Kind::Remote => self.test_remote(&connection).await?,
            Kind::Ssh => self.test_ssh(&connection).await,
        };
        report.latched = self.connections.shared.latched(id);
        Ok(report)
    }

    async fn test_local(&self) -> TestReport {
        match self.local_profiles().await {
            Ok(profiles) => TestReport {
                health: Step::pass(format!("the local daemon serves {}", profiles.join(", "))),
                websocket: Step::pass("dialed with one-use tickets of the control channel"),
                confirm: None,
                latched: None,
            },
            Err(e) => TestReport::stopped(Step::fail(e)),
        }
    }

    async fn test_remote(&self, connection: &Connection) -> Result<TestReport, String> {
        let url =
            Url::parse(connection.url.as_deref().ok_or("no URL")?).map_err(|e| e.to_string())?;
        let pin = self.connections.shared.pins.get(&connection.id);
        // Classify the TLS step first: an unpinned self-signed server is a
        // key to confirm, a changed one a latch.
        match tls::dial(&url, pin.as_deref()).await {
            Err(DialFailure::Untrusted { fingerprint }) => {
                let mut report =
                    TestReport::stopped(Step::fail("the certificate is not trusted yet"));
                report.confirm = Some(Confirm {
                    kind: "certificate",
                    fingerprint,
                });
                return Ok(report);
            }
            Err(failure) => {
                let shared = &self.connections.shared;
                let message = super::dialing::dial_error(shared, &connection.id, &failure);
                return Ok(TestReport::stopped(Step::fail(message)));
            }
            Ok(_) => {}
        }
        let health = match self
            .remote_request(connection, "GET", "/api/health", None, false)
            .await
        {
            Ok(reply) if reply.status == 200 => Step::pass(format!(
                "Prometheus {} answers",
                reply.json()["version"].as_str().unwrap_or("?")
            )),
            Ok(reply) => Step::fail(format!("/api/health answered {}", reply.status)),
            Err(e) => Step::fail(e),
        };
        let websocket = self.handshake(connection).await;
        Ok(TestReport {
            health,
            websocket,
            confirm: None,
            latched: None,
        })
    }

    async fn test_ssh(&self, connection: &Connection) -> TestReport {
        let Ok(ssh) = self.connections.ssh() else {
            return TestReport::stopped(Step::fail("this shell has no ssh connections"));
        };
        let Some(target) = &connection.ssh else {
            return TestReport::stopped(Step::fail("no ssh target"));
        };
        let health = match ssh.ensure(target).await {
            Ok(report) => Step::pass(format!(
                "Prometheus {} runs its daemon at {}",
                report.version, report.rpc_url
            )),
            Err(SshFailure::HostKeyUnknown { fingerprint }) => {
                let mut report = TestReport::stopped(Step::fail("the host key is not trusted yet"));
                report.confirm = Some(Confirm {
                    kind: "host_key",
                    fingerprint,
                });
                return report;
            }
            Err(failure) => {
                let message =
                    super::dialing::ssh_error(&self.connections.shared, &connection.id, &failure);
                return TestReport::stopped(Step::fail(message));
            }
        };
        TestReport {
            health,
            websocket: self.handshake(connection).await,
            confirm: None,
            latched: None,
        }
    }

    async fn handshake(&self, connection: &Connection) -> Step {
        match self.socket(connection).await {
            Ok(socket) => {
                socket.close().await;
                Step::pass("the client protocol answered rpc.hello")
            }
            Err(e) => Step::fail(e),
        }
    }

    /// Makes `id` ready to open an agent on: the tunnel of an `ssh`
    /// connection, the reachability of a `remote` one.
    pub(crate) async fn warm(&self, id: &str) -> Result<(), String> {
        let connection = self.connections.get(id)?;
        match connection.kind {
            Kind::Local => self.local_profiles().await.map(drop),
            Kind::Remote => self
                .remote_request(&connection, "GET", "/api/health", None, false)
                .await
                .map(drop),
            Kind::Ssh => self
                .dial_connection(
                    "warm",
                    id,
                    "default",
                    crate::endpoint::RPC_PATH,
                    crate::dial::DialPurpose::Interactive,
                )
                .await
                .map(|dial| {
                    let _ = self.dials.settle("warm", &dial.dial);
                }),
        }
    }

    pub(crate) async fn local_profiles(&self) -> Result<Vec<String>, String> {
        let endpoint = self
            .rediscover()
            .or_else(|| self.current())
            .ok_or("the local daemon is not running")?;
        crate::shell::daemon_status(&endpoint.control)
            .await
            .map(|s| s.profiles)
            .ok_or_else(|| "the local daemon did not answer".to_owned())
    }

    async fn profiles_of(&self, connection: &Connection) -> Result<Vec<String>, String> {
        match connection.kind {
            Kind::Local => self.local_profiles().await,
            Kind::Remote => {
                let reply = self
                    .remote_request(connection, "GET", "/api/profiles", None, true)
                    .await?;
                if reply.status != 200 {
                    return Err(format!("the server answered {}", reply.status));
                }
                Ok(names(&reply.json()["profiles"], |p| p["name"].as_str()))
            }
            Kind::Ssh => {
                let target = connection.ssh.as_ref().ok_or("no ssh target")?;
                let status = self.connections.ssh()?.status(target).await.map_err(|f| {
                    super::dialing::ssh_error(&self.connections.shared, &connection.id, &f)
                })?;
                if status["running"] != json!(true) {
                    return Err("the daemon is not running on the host".to_owned());
                }
                Ok(names(&status["daemon"]["profiles"], Value::as_str))
            }
        }
    }

    /// The union roster of every connection's profiles.
    pub(crate) async fn roster(&self) -> Roster {
        let connections = self.connections.with(|r| r.listing().connections);
        let listed = join_all(connections.iter().map(|c| async move {
            Listed {
                id: c.id.clone(),
                name: c.name.clone(),
                profiles: self.profiles_of(c).await,
            }
        }))
        .await;
        roster::build(&listed)
    }

    async fn update_remote(&self, connection: &Connection) -> UpdateRow {
        let row = |outcome: &str, message: String| UpdateRow {
            connection_id: connection.id.clone(),
            name: connection.name.clone(),
            outcome: outcome.to_owned(),
            message,
        };
        let mut socket = match self.socket(connection).await {
            Ok(socket) => socket,
            Err(e) => return row("unreachable", e),
        };
        let answer = socket.call("daemon.update", json!({})).await;
        socket.close().await;
        match answer {
            Ok(result) => row(
                result["status"].as_str().unwrap_or("started"),
                result["message"].as_str().unwrap_or_default().to_owned(),
            ),
            Err(message) => row("refused", message),
        }
    }

    async fn update_app(&self) -> UpdateRow {
        let row = |outcome: &str, message: Option<String>| UpdateRow {
            connection_id: super::LOCAL.to_owned(),
            name: "This app".to_owned(),
            outcome: outcome.to_owned(),
            message: message.unwrap_or_default(),
        };
        let checked = self.updater.check(true).await;
        if checked.mechanism == Mechanism::None {
            return row("unavailable", checked.message);
        }
        if !checked.ok {
            return row("failed", checked.message);
        }
        if checked.available.is_none() {
            return row("up_to_date", checked.message);
        }
        let applied = self.updater.apply().await;
        row(
            if applied.ok { "staged" } else { "failed" },
            applied.message,
        )
    }

    /// Update all instances: every other connection updates itself in
    /// parallel; the app (and with it the local daemon it restarts when
    /// stale) updates last.
    pub(crate) async fn update_all(&self) -> Vec<UpdateRow> {
        let connections = self.connections.with(|r| r.listing().connections);
        let mut rows = join_all(
            connections
                .iter()
                .filter(|c| c.kind != Kind::Local)
                .map(|c| self.update_remote(c)),
        )
        .await;
        rows.push(self.update_app().await);
        rows
    }
}

fn names<'a>(list: &'a Value, name: impl Fn(&'a Value) -> Option<&'a str>) -> Vec<String> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(name)
        .map(str::to_owned)
        .collect()
}
