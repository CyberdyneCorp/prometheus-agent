//! The HTTP client of the Nous connector gateway.
//!
//! Every request carries the profile's Nous sign-in as a bearer token,
//! read when the request is made (a refreshed token is picked up). Only
//! execute is retried, and only under its own `Idempotency-Key`, so a
//! retried call cannot run twice; authorization starts are never retried.

use std::sync::Arc;
use std::time::Duration;

use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use super::wire::{
    self, Account, ConnectRequest, ConnectResult, ConnectorItem, GatewayFailure, ToolSchema,
};

/// Bound on one `tool_search` gateway search.
pub const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound on every other request but execute.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound on one execute.
const EXECUTE_TIMEOUT: Duration = Duration::from_mins(1);

/// Reads the profile's Nous access token (`None`: not signed in).
pub type TokenSource = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// Why a gateway request failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayError {
    /// No Nous sign-in.
    NoToken,
    /// Transport failure, timeout or an unreadable answer.
    Unreachable(String),
    /// The gateway answered an error status.
    Refused {
        /// HTTP status.
        status: u16,
        /// The error body (code empty when the body had none).
        failure: GatewayFailure,
    },
}

impl GatewayError {
    /// The gateway's error code, when it gave one.
    #[must_use]
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Refused { failure, .. } if !failure.code.is_empty() => Some(&failure.code),
            _ => None,
        }
    }
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoToken => f.write_str("no Nous sign-in"),
            Self::Unreachable(why) => write!(f, "connector gateway unreachable: {why}"),
            Self::Refused { status, failure } if failure.message.is_empty() => {
                write!(f, "connector gateway answered {status}")
            }
            Self::Refused { status, failure } => {
                write!(
                    f,
                    "connector gateway answered {status}: {}",
                    failure.message
                )
            }
        }
    }
}

/// One connector gateway, as one signed-in profile sees it.
#[derive(Clone)]
pub struct Gateway {
    origin: String,
    http: reqwest::Client,
    token: TokenSource,
    search_timeout: Duration,
}

impl std::fmt::Debug for Gateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gateway")
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

/// What one execute answered.
#[derive(Debug, Clone, PartialEq)]
pub struct Executed {
    /// The tool's data.
    pub data: Value,
    /// The alias of the account it ran under (`None`: unnamed).
    pub account: Option<String>,
}

impl Gateway {
    /// A client of the gateway at `origin` over `http`, authenticated with
    /// `token`.
    #[must_use]
    pub fn new(origin: &str, http: reqwest::Client, token: TokenSource) -> Self {
        Self {
            origin: origin.trim_end_matches('/').to_owned(),
            http,
            token,
            search_timeout: SEARCH_TIMEOUT,
        }
    }

    /// Bounds searches by `timeout` instead of [`SEARCH_TIMEOUT`] (tests).
    #[must_use]
    pub fn with_search_timeout(mut self, timeout: Duration) -> Self {
        self.search_timeout = timeout;
        self
    }

    /// The gateway's origin.
    #[must_use]
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// Whether a sign-in token is available now.
    #[must_use]
    pub fn signed_in(&self) -> bool {
        (self.token)().is_some()
    }

    /// The schemas of the remote tools matching `queries`, keyed by vendor
    /// slug (bounded by the search timeout).
    ///
    /// # Errors
    ///
    /// Any [`GatewayError`], a timeout included.
    pub async fn search(&self, queries: &[String]) -> Result<Vec<ToolSchema>, GatewayError> {
        let body =
            json!({"queries": queries.iter().map(|q| json!({"useCase": q})).collect::<Vec<_>>()});
        let answer: wire::SearchResponse = self
            .send(
                Method::POST,
                wire::SEARCH,
                Some(&body),
                self.search_timeout,
                None,
            )
            .await?;
        Ok(schemas(answer.schemas))
    }

    /// The schemas of vendor slugs `tools`.
    ///
    /// # Errors
    ///
    /// Any [`GatewayError`].
    pub async fn schemas(&self, tools: &[String]) -> Result<Vec<ToolSchema>, GatewayError> {
        let answer: wire::SchemasResponse = self
            .send(
                Method::POST,
                wire::SCHEMAS,
                Some(&json!({"tools": tools})),
                REQUEST_TIMEOUT,
                None,
            )
            .await?;
        Ok(schemas(answer.schemas))
    }

    /// Runs vendor tool `tool` of `connector` once (retried once under the
    /// same idempotency key when the gateway is briefly unavailable).
    ///
    /// # Errors
    ///
    /// Any [`GatewayError`]; a refused call carries the gateway's code
    /// (`CONNECTION_REQUIRED`, `TOOL_NOT_FOUND`...).
    pub async fn execute(
        &self,
        connector: &str,
        tool: &str,
        arguments: &Value,
    ) -> Result<Executed, GatewayError> {
        let body = json!({"call": {"connector": connector, "tool": tool, "arguments": arguments}});
        let key = ulid::Ulid::generate().to_string();
        let mut attempt = 0;
        loop {
            let outcome: Result<wire::ExecuteResponse, _> = self
                .send(
                    Method::POST,
                    wire::EXECUTE,
                    Some(&body),
                    EXECUTE_TIMEOUT,
                    Some(&key),
                )
                .await;
            match outcome {
                Ok(answer) => {
                    return Ok(Executed {
                        data: answer.result.data,
                        account: answer.result.account,
                    });
                }
                Err(GatewayError::Refused { status, .. })
                    if attempt == 0 && matches!(status, 502..=504) =>
                {
                    attempt += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Starts (or with `reinitiate`, repairs) authorizations.
    ///
    /// # Errors
    ///
    /// Any [`GatewayError`].
    pub async fn connect(
        &self,
        request: &ConnectRequest,
    ) -> Result<Vec<ConnectResult>, GatewayError> {
        let body = serde_json::to_value(request).unwrap_or(Value::Null);
        let answer: wire::ConnectResponse = self
            .send(
                Method::POST,
                wire::CONNECTIONS,
                Some(&body),
                REQUEST_TIMEOUT,
                None,
            )
            .await?;
        Ok(answer.results)
    }

    /// The connectors this account may use, with their connected state.
    ///
    /// # Errors
    ///
    /// Any [`GatewayError`].
    pub async fn list(&self) -> Result<Vec<ConnectorItem>, GatewayError> {
        let answer: wire::ListResponse = self
            .send(Method::GET, wire::LIST, None, REQUEST_TIMEOUT, None)
            .await?;
        Ok(answer.items)
    }

    /// Every account (retired ones included).
    ///
    /// # Errors
    ///
    /// Any [`GatewayError`].
    pub async fn accounts(&self) -> Result<Vec<Account>, GatewayError> {
        let answer: wire::AccountsResponse = self
            .send(Method::GET, wire::ACCOUNTS, None, REQUEST_TIMEOUT, None)
            .await?;
        Ok(answer.accounts)
    }

    /// Sets the alias of account `id`.
    ///
    /// # Errors
    ///
    /// Any [`GatewayError`]; a gateway without the route answers 404, 405
    /// or 501.
    pub async fn rename(&self, id: &str, alias: &str) -> Result<(), GatewayError> {
        let path = format!("{}/{}", wire::ACCOUNTS, encode(id));
        let _: Value = self
            .send(
                Method::PATCH,
                &path,
                Some(&json!({"alias": alias})),
                REQUEST_TIMEOUT,
                None,
            )
            .await?;
        Ok(())
    }

    /// Removes account `id`.
    ///
    /// # Errors
    ///
    /// Any [`GatewayError`].
    pub async fn remove(&self, id: &str) -> Result<(), GatewayError> {
        let path = format!("{}/{}", wire::ACCOUNTS, encode(id));
        let _: Value = self
            .send(Method::DELETE, &path, None, REQUEST_TIMEOUT, None)
            .await?;
        Ok(())
    }

    async fn send<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        timeout: Duration,
        idempotency_key: Option<&str>,
    ) -> Result<T, GatewayError> {
        let token = (self.token)().ok_or(GatewayError::NoToken)?;
        let url = format!("{}/{path}", self.origin);
        let mut request = self
            .http
            .request(method, &url)
            .bearer_auth(token)
            .timeout(timeout);
        if let Some(body) = body {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.to_string());
        }
        if let Some(key) = idempotency_key {
            request = request.header("Idempotency-Key", key);
        }
        let work = async {
            let response = request
                .send()
                .await
                .map_err(|e| GatewayError::Unreachable(e.without_url().to_string()))?;
            let status = response.status();
            let bytes = response
                .bytes()
                .await
                .map_err(|e| GatewayError::Unreachable(e.without_url().to_string()))?;
            Ok::<_, GatewayError>((status, bytes))
        };
        let (status, bytes) = tokio::time::timeout(timeout, work).await.map_err(|_| {
            GatewayError::Unreachable(format!("no answer within {}s", timeout.as_secs()))
        })??;
        if !status.is_success() {
            return Err(refused(status, &bytes));
        }
        let bytes: &[u8] = if bytes.is_empty() { b"null" } else { &bytes };
        serde_json::from_slice(bytes)
            .map_err(|e| GatewayError::Unreachable(format!("unreadable answer: {e}")))
    }
}

fn refused(status: StatusCode, body: &[u8]) -> GatewayError {
    let value: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let inner = value
        .get("error")
        .filter(|e| e.is_object())
        .unwrap_or(&value);
    let failure = serde_json::from_value(inner.clone()).unwrap_or_default();
    GatewayError::Refused {
        status: status.as_u16(),
        failure,
    }
}

/// The well-formed schemas of `map` (a malformed one is skipped).
fn schemas(map: Map<String, Value>) -> Vec<ToolSchema> {
    map.into_iter()
        .filter_map(|(key, value)| match serde_json::from_value(value) {
            Ok(schema) => Some(schema),
            Err(err) => {
                tracing::debug!(key, "connector schema skipped: {err}");
                None
            }
        })
        .collect()
}

fn encode(segment: &str) -> String {
    percent_encoding::utf8_percent_encode(segment, percent_encoding::NON_ALPHANUMERIC).to_string()
}
