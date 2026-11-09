//! The first-party cloud browser providers: each creates a session over
//! its vendor's REST API and releases it at the end.

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::{Value, json};

use super::http::{Endpoint, HttpClient, TokenSource, failure, field, http, send};
use super::{CloudProvider, CloudSession, CreateFuture, Manifest, ReleaseFuture, manifest};
use crate::web::SecretLookup;

fn manifest_of(id: &str) -> &'static Manifest {
    manifest(id).expect("a first-party provider has a manifest")
}

/// The secret `name`, or the error naming it.
fn required(secrets: &SecretLookup, provider: &str, name: &str) -> Result<String, String> {
    secrets(name)
        .map(|s| s.expose().to_owned())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            format!("{provider} needs the secret {name}: run 'prometheus secrets set {name}'")
        })
}

/// The created session in `body` (its id and DevTools URL under one of
/// `url_keys`), checked against the provider's hosts.
fn created(
    endpoint: &Endpoint,
    manifest: &Manifest,
    body: &Value,
    url_keys: &[&str],
) -> Result<CloudSession, String> {
    let id = field(manifest.id, body, &["id"])?;
    let url = field(manifest.id, body, url_keys)?;
    endpoint.check(manifest, &url)?;
    Ok(CloudSession { id, endpoint: url })
}

/// Browserbase: `BROWSERBASE_API_KEY` and `BROWSERBASE_PROJECT_ID`
/// (`BROWSERBASE_BASE_URL` overrides the API). Sessions ask for
/// keep-alive and proxies; a plan without them (HTTP 402) is retried
/// without keep-alive, then without proxies.
pub struct Browserbase {
    client: HttpClient,
    secrets: SecretLookup,
}

impl Browserbase {
    /// The provider over `client`.
    #[must_use]
    pub fn new(client: HttpClient, secrets: SecretLookup) -> Self {
        Self { client, secrets }
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint::of(
            "https://api.browserbase.com",
            (self.secrets)("BROWSERBASE_BASE_URL"),
        )
    }

    async fn create_session(&self) -> Result<CloudSession, String> {
        let manifest = manifest_of("browserbase");
        let key = required(&self.secrets, manifest.id, "BROWSERBASE_API_KEY")?;
        let project = required(&self.secrets, manifest.id, "BROWSERBASE_PROJECT_ID")?;
        let endpoint = self.endpoint();
        let url = endpoint.url("/v1/sessions");
        endpoint.check(manifest, &url)?;
        let mut body = json!({"projectId": project, "keepAlive": true, "proxies": true});
        let mut reply = self.post(&url, &key, &body).await?;
        for paid in ["keepAlive", "proxies"] {
            if reply.0 != 402 {
                break;
            }
            tracing::warn!(
                feature = paid,
                "Browserbase plan lacks {paid}; retrying without it"
            );
            body.as_object_mut().map(|o| o.remove(paid));
            reply = self.post(&url, &key, &body).await?;
        }
        let (status, answer) = reply;
        if !(200..300).contains(&status) {
            return Err(failure(manifest.id, status, &answer));
        }
        created(&endpoint, manifest, &answer, &["connectUrl"])
    }

    async fn post(&self, url: &str, key: &str, body: &Value) -> Result<(u16, Value), String> {
        send(
            http(&self.client)?
                .post(url)
                .header("X-BB-API-Key", key)
                .header(CONTENT_TYPE, "application/json")
                .body(body.to_string()),
        )
        .await
    }

    async fn release_session(&self, id: &str) -> Result<(), String> {
        let manifest = manifest_of("browserbase");
        let key = required(&self.secrets, manifest.id, "BROWSERBASE_API_KEY")?;
        let project = required(&self.secrets, manifest.id, "BROWSERBASE_PROJECT_ID")?;
        let endpoint = self.endpoint();
        let url = endpoint.url(&format!("/v1/sessions/{id}"));
        endpoint.check(manifest, &url)?;
        let body = json!({"projectId": project, "status": "REQUEST_RELEASE"});
        let (status, answer) = self.post(&url, &key, &body).await?;
        ok_or_failure(manifest.id, status, &answer)
    }
}

impl CloudProvider for Browserbase {
    fn create(&self) -> CreateFuture<'_> {
        Box::pin(self.create_session())
    }

    fn release<'a>(&'a self, id: &'a str) -> ReleaseFuture<'a> {
        Box::pin(self.release_session(id))
    }
}

fn ok_or_failure(provider: &str, status: u16, body: &Value) -> Result<(), String> {
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(failure(provider, status, body))
    }
}

/// The Browser Use API shared by `browser-use` (the user's key) and
/// `subscription` (the tool gateway, billed to the subscription).
struct BrowserUseApi {
    client: HttpClient,
    endpoint: Endpoint,
    manifest: &'static Manifest,
}

impl BrowserUseApi {
    async fn create(
        &self,
        key: &str,
        body: Value,
        idempotency: Option<&str>,
    ) -> Result<CloudSession, String> {
        let url = self.endpoint.url("/browsers");
        self.endpoint.check(self.manifest, &url)?;
        let mut request = http(&self.client)?
            .post(&url)
            .header("X-Browser-Use-API-Key", key)
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_string());
        if let Some(idempotency) = idempotency {
            request = request.header("X-Idempotency-Key", idempotency);
        }
        let (status, answer) = send(request).await?;
        if !(200..300).contains(&status) {
            return Err(failure(self.manifest.id, status, &answer));
        }
        created(
            &self.endpoint,
            self.manifest,
            &answer,
            &["cdpUrl", "connectUrl"],
        )
    }

    async fn release(&self, key: &str, id: &str) -> Result<(), String> {
        let url = self.endpoint.url(&format!("/browsers/{id}"));
        self.endpoint.check(self.manifest, &url)?;
        let (status, answer) = send(
            http(&self.client)?
                .patch(&url)
                .header("X-Browser-Use-API-Key", key)
                .header(CONTENT_TYPE, "application/json")
                .body(json!({"action": "stop"}).to_string()),
        )
        .await?;
        ok_or_failure(self.manifest.id, status, &answer)
    }
}

/// Browser Use with the user's `BROWSER_USE_API_KEY`.
pub struct BrowserUse {
    api: BrowserUseApi,
    secrets: SecretLookup,
}

impl BrowserUse {
    /// The provider over `client`.
    #[must_use]
    pub fn new(client: HttpClient, secrets: SecretLookup) -> Self {
        Self::at(
            client,
            secrets,
            Endpoint::of("https://api.browser-use.com/api/v3", None),
        )
    }

    /// The provider of the API at `endpoint` (a test double's).
    #[must_use]
    pub fn at(client: HttpClient, secrets: SecretLookup, endpoint: Endpoint) -> Self {
        Self {
            api: BrowserUseApi {
                client,
                endpoint,
                manifest: manifest_of("browser-use"),
            },
            secrets,
        }
    }

    fn key(&self) -> Result<String, String> {
        required(&self.secrets, "browser-use", "BROWSER_USE_API_KEY")
    }
}

impl CloudProvider for BrowserUse {
    fn create(&self) -> CreateFuture<'_> {
        Box::pin(async move { self.api.create(&self.key()?, json!({}), None).await })
    }

    fn release<'a>(&'a self, id: &'a str) -> ReleaseFuture<'a> {
        Box::pin(async move { self.api.release(&self.key()?, id).await })
    }
}

/// The tool gateway origin of the subscription's cloud browsers.
pub const GATEWAY_ORIGIN: &str = "https://browser-use-gateway.nousresearch.com";

/// Cloud browsers through the signed-in subscription's tool gateway: the
/// Browser Use API with the subscription token, short sessions (the
/// gateway bills per minute), one idempotency key per create so a retried
/// request never opens a second browser.
pub struct Subscription {
    api: BrowserUseApi,
    token: TokenSource,
}

impl Subscription {
    /// The provider over `client` and the subscription's `token`.
    #[must_use]
    pub fn new(client: HttpClient, token: TokenSource) -> Self {
        Self::at(client, token, Endpoint::of(GATEWAY_ORIGIN, None))
    }

    /// The provider of the gateway at `endpoint` (a self-hosted gateway or
    /// a test double).
    #[must_use]
    pub fn at(client: HttpClient, token: TokenSource, endpoint: Endpoint) -> Self {
        Self {
            api: BrowserUseApi {
                client,
                endpoint,
                manifest: manifest_of(super::SUBSCRIPTION),
            },
            token,
        }
    }

    fn token(&self) -> Result<String, String> {
        (self.token)()
            .map(|t| t.expose().to_owned())
            .ok_or_else(|| {
                "browser.provider = \"subscription\" needs a signed-in provider subscription: run 'prometheus auth add nous'"
                    .to_owned()
            })
    }
}

impl CloudProvider for Subscription {
    fn create(&self) -> CreateFuture<'_> {
        Box::pin(async move {
            let key = format!(
                "browser-session-create:{}",
                ulid::Ulid::generate().to_string().to_lowercase()
            );
            let body = json!({"timeout": 5, "proxyCountryCode": "us"});
            self.api.create(&self.token()?, body, Some(&key)).await
        })
    }

    fn release<'a>(&'a self, id: &'a str) -> ReleaseFuture<'a> {
        Box::pin(async move { self.api.release(&self.token()?, id).await })
    }
}

/// Firecrawl: `FIRECRAWL_API_KEY` (`FIRECRAWL_API_URL` overrides the
/// API); sessions live 300 s unless released.
pub struct Firecrawl {
    client: HttpClient,
    secrets: SecretLookup,
}

impl Firecrawl {
    /// The provider over `client`.
    #[must_use]
    pub fn new(client: HttpClient, secrets: SecretLookup) -> Self {
        Self { client, secrets }
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint::of(
            "https://api.firecrawl.dev",
            (self.secrets)("FIRECRAWL_API_URL"),
        )
    }

    fn bearer(&self) -> Result<String, String> {
        required(&self.secrets, "firecrawl", "FIRECRAWL_API_KEY").map(|k| format!("Bearer {k}"))
    }

    async fn create_session(&self) -> Result<CloudSession, String> {
        let manifest = manifest_of("firecrawl");
        let bearer = self.bearer()?;
        let endpoint = self.endpoint();
        let url = endpoint.url("/v2/browser");
        endpoint.check(manifest, &url)?;
        let (status, answer) = send(
            http(&self.client)?
                .post(&url)
                .header(AUTHORIZATION, bearer)
                .header(CONTENT_TYPE, "application/json")
                .body(json!({"ttl": 300}).to_string()),
        )
        .await?;
        if !(200..300).contains(&status) {
            return Err(failure(manifest.id, status, &answer));
        }
        created(&endpoint, manifest, &answer, &["cdpUrl"])
    }

    async fn release_session(&self, id: &str) -> Result<(), String> {
        let manifest = manifest_of("firecrawl");
        let bearer = self.bearer()?;
        let endpoint = self.endpoint();
        let url = endpoint.url(&format!("/v2/browser/{id}"));
        endpoint.check(manifest, &url)?;
        let (status, answer) = send(
            http(&self.client)?
                .delete(&url)
                .header(AUTHORIZATION, bearer),
        )
        .await?;
        ok_or_failure(manifest.id, status, &answer)
    }
}

impl CloudProvider for Firecrawl {
    fn create(&self) -> CreateFuture<'_> {
        Box::pin(self.create_session())
    }

    fn release<'a>(&'a self, id: &'a str) -> ReleaseFuture<'a> {
        Box::pin(self.release_session(id))
    }
}
