//! A peer gateway's API server, as this machine calls it (spec:
//! agent-interop, Peer gateway registry; Peer direct messages and remote
//! runs; Union roster across gateways). The key comes from this profile's
//! secrets store and is sent only to the peer's own URL; the peer's
//! `/p/<agent>/` prefix names an agent on a multiplexed peer.

use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_config::schema::{PeerGateway, valid_peer_name};
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use serde_json::{Value, json};

/// How long one request to a peer may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The largest peer answer read.
const MAX_BODY: usize = 8 * 1024 * 1024;

/// Why a peer call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PeerError {
    /// The connection could not be made.
    Unreachable(String),
    /// The peer answered 401 or 403.
    KeyRejected,
    /// The peer answered 404 (no such agent or run).
    NotFound(String),
    /// The peer refused the request otherwise.
    Rejected(u16, String),
}

/// A registered peer.
pub(crate) struct Peer {
    /// Its name.
    pub(crate) name: String,
    base: String,
    key: SecretString,
    http: reqwest::Client,
}

impl Peer {
    /// The registered peer `name` of `scope`, with its key.
    ///
    /// # Errors
    ///
    /// `peer.invalid_name`, `peer.not_found`, `peer.key_rejected` when its
    /// secret is missing.
    pub(crate) fn of(scope: &ProfileScope, name: &str) -> Result<Self, Error> {
        if !valid_peer_name(name) {
            return Err(invalid_name(name));
        }
        let config = scope.config();
        let entry = config.peers.get(name).ok_or_else(|| {
            Error::new(&codes::PEER_NOT_FOUND, format!("no peer named {name}"))
                .with_hint("prometheus gateway peer list")
        })?;
        let key = scope.secrets().resolve(&entry.key_secret.0).map_err(|_| {
            Error::new(
                &codes::PEER_KEY_REJECTED,
                format!(
                    "the key of peer {name} is missing (secret {})",
                    entry.key_secret.0
                ),
            )
        })?;
        Self::new(scope, name, entry, key)
    }

    /// A client of `entry` with `key`.
    ///
    /// # Errors
    ///
    /// The HTTP client cannot be built.
    pub(crate) fn new(
        scope: &ProfileScope,
        name: &str,
        entry: &PeerGateway,
        key: SecretString,
    ) -> Result<Self, Error> {
        let config = scope.config();
        let outbound = pmt_net::OutboundPolicy {
            force_ipv4: config.network.force_ipv4,
            extra_ca_file: config.network.extra_ca_file.as_ref().map(Into::into),
            proxy: config.network.proxy.clone(),
        };
        let http = pmt_net::client_builder(&outbound)?
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| Error::new(&codes::NET_CLIENT_INIT, e.to_string()))?;
        Ok(Self {
            name: name.to_owned(),
            base: entry.url.trim_end_matches('/').to_owned(),
            key,
            http,
        })
    }

    fn url(&self, agent: Option<&str>, path: &str) -> String {
        match agent {
            Some(agent) => format!("{}/p/{agent}{path}", self.base),
            None => format!("{}{path}", self.base),
        }
    }

    async fn send(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<&Value>,
        idempotency: Option<&str>,
    ) -> Result<Value, PeerError> {
        let mut request = self
            .http
            .request(method, url)
            .bearer_auth(self.key.expose());
        if let Some(key) = idempotency {
            request = request.header("Idempotency-Key", key);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(|e| PeerError::Unreachable(e.to_string()))?;
        let status = response.status().as_u16();
        let bytes = pmt_net::fetch::read_capped(response, MAX_BODY)
            .await
            .map_err(|e| PeerError::Rejected(status, format!("the answer: {e}")))?;
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let message = || {
            body["error"]["message"]
                .as_str()
                .or_else(|| body["error"].as_str())
                .unwrap_or("no message")
                .to_owned()
        };
        match status {
            200..=299 => Ok(body),
            401 | 403 => Err(PeerError::KeyRejected),
            404 => Err(PeerError::NotFound(message())),
            _ => Err(PeerError::Rejected(status, message())),
        }
    }

    /// `GET /v1/capabilities`.
    ///
    /// # Errors
    ///
    /// As the peer answers.
    pub(crate) async fn probe(&self) -> Result<Value, PeerError> {
        self.send(
            reqwest::Method::GET,
            &self.url(None, "/v1/capabilities"),
            None,
            None,
        )
        .await
    }

    /// The canonical Bot Chat of `agent` (the peer's default profile when
    /// `None`).
    ///
    /// # Errors
    ///
    /// As the peer answers.
    pub(crate) async fn ensure_bot_chat(&self, agent: Option<&str>) -> Result<String, PeerError> {
        let body = self
            .send(
                reqwest::Method::POST,
                &self.url(agent, "/api/bot-chat/ensure"),
                Some(&json!({})),
                None,
            )
            .await?;
        body["session_id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| PeerError::Rejected(200, "the peer named no Bot Chat".to_owned()))
    }

    /// Submits `message` as a run in session `session` of `agent`.
    ///
    /// # Errors
    ///
    /// As the peer answers.
    pub(crate) async fn run(
        &self,
        agent: Option<&str>,
        session: &str,
        message: &str,
        idempotency: &str,
    ) -> Result<Value, PeerError> {
        let body = json!({"session_id": session, "input": message});
        self.send(
            reqwest::Method::POST,
            &self.url(agent, "/v1/runs"),
            Some(&body),
            Some(idempotency),
        )
        .await
    }

    /// Run `run_id` of `agent`.
    ///
    /// # Errors
    ///
    /// As the peer answers.
    pub(crate) async fn status(
        &self,
        agent: Option<&str>,
        run_id: &str,
    ) -> Result<Value, PeerError> {
        let path = format!("/v1/runs/{run_id}");
        self.send(reqwest::Method::GET, &self.url(agent, &path), None, None)
            .await
    }

    /// Stops run `run_id` of `agent`.
    ///
    /// # Errors
    ///
    /// As the peer answers.
    pub(crate) async fn stop(&self, agent: Option<&str>, run_id: &str) -> Result<Value, PeerError> {
        let path = format!("/v1/runs/{run_id}/stop");
        self.send(
            reqwest::Method::POST,
            &self.url(agent, &path),
            Some(&json!({})),
            None,
        )
        .await
    }

    /// `method` `path` of `agent` (`None`: the peer's default profile),
    /// for the Bot Mode endpoints (`/api/bots`, `/api/rooms/...`).
    ///
    /// # Errors
    ///
    /// As the peer answers.
    pub(crate) async fn call(
        &self,
        method: reqwest::Method,
        agent: Option<&str>,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, PeerError> {
        self.send(method, &self.url(agent, path), body, None).await
    }

    /// Starts a room authority's run of `body["member"]` on this peer,
    /// under the room grant the peer issued (`X-Prometheus-Room-Grant`).
    ///
    /// # Errors
    ///
    /// As the peer answers: 403 ([`PeerError::KeyRejected`]) without a
    /// valid grant.
    pub(crate) async fn room_turn(
        &self,
        room: &str,
        body: &Value,
        grant: Option<String>,
    ) -> Result<Value, PeerError> {
        let url = self.url(None, &format!("/api/rooms/{room}/turns"));
        let mut request = self
            .http
            .post(url)
            .bearer_auth(self.key.expose())
            .json(body);
        if let Some(grant) = grant {
            request = request.header("X-Prometheus-Room-Grant", grant);
        }
        let response = request
            .send()
            .await
            .map_err(|e| PeerError::Unreachable(e.to_string()))?;
        match response.status().as_u16() {
            200..=299 => Ok(response.json().await.unwrap_or(Value::Null)),
            401 | 403 => Err(PeerError::KeyRejected),
            status => Err(PeerError::Rejected(
                status,
                "the room run was refused".to_owned(),
            )),
        }
    }

    /// `GET /api/agents`.
    ///
    /// # Errors
    ///
    /// As the peer answers.
    pub(crate) async fn agents(&self) -> Result<Value, PeerError> {
        self.send(
            reqwest::Method::GET,
            &self.url(None, "/api/agents"),
            None,
            None,
        )
        .await
    }
}

/// The refusal of a malformed peer name.
pub(crate) fn invalid_name(name: &str) -> Error {
    Error::new(
        &codes::PEER_INVALID_NAME,
        format!("'{name}' is not a peer name (^[a-z][a-z0-9-]{{0,31}}$)"),
    )
}

impl PeerError {
    /// As an error of peer `name`, with its exit code.
    #[must_use]
    pub(crate) fn into_error(self, name: &str) -> Error {
        match self {
            Self::Unreachable(why) => Error::new(
                &codes::PEER_UNREACHABLE,
                format!("peer {name} unreachable: {why}"),
            ),
            Self::KeyRejected => Error::new(
                &codes::PEER_KEY_REJECTED,
                format!("peer {name} rejected the key"),
            ),
            Self::NotFound(why) => {
                Error::new(&codes::PEER_NOT_FOUND, format!("peer {name}: {why}"))
            }
            Self::Rejected(status, why) => Error::new(
                &codes::PEER_REJECTED,
                format!("peer {name} refused the request (HTTP {status}): {why}"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use pmt_config::schema::SecretName;
    use pmt_config::{Home, Launch, Overrides, ProfileName};

    use super::*;

    /// A peer streaming an endless answer is cut off at the cap instead of
    /// being buffered whole.
    #[tokio::test]
    async fn an_endless_peer_answer_is_refused_at_the_cap() {
        let root = tempfile::tempdir().unwrap();
        let home = Home::open(root.path()).unwrap();
        let profile = home.profile(&ProfileName::default_profile()).unwrap();
        let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap();
        let entry = PeerGateway {
            url: crate::a2a_host::transport::endless_server().await,
            note: None,
            key_secret: SecretName("PEER_LAB_KEY".to_owned()),
        };
        let peer = Peer::new(&scope, "lab", &entry, SecretString::new("k")).unwrap();
        let refused = peer.probe().await.unwrap_err();
        assert_eq!(
            refused,
            PeerError::Rejected(200, "the answer: larger than 8 MiB".to_owned())
        );
    }
}
