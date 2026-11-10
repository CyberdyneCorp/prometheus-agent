//! The AWS credential chain of the `bedrock` provider, in the order the
//! AWS SDKs use: environment secrets (`AWS_ACCESS_KEY_ID`,
//! `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`), the shared profile
//! `providers.<id>.aws_profile` (static keys, or an IAM Identity Center
//! sign-in exchanged for role credentials), container credentials, then
//! the instance role through `IMDSv2`. Nothing is read from non-secret
//! environment variables: the container endpoint variables are imported
//! as secrets like the keys.

use std::path::PathBuf;
use std::time::Duration;

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use serde_json::Value;

use super::Keys;
use super::profile::{self, Sso};
use crate::registry::Provider;
use crate::resolve::SecretSource;

/// The access key id secret.
pub const ACCESS_KEY_ID: &str = "AWS_ACCESS_KEY_ID";
/// The secret access key secret.
pub const SECRET_ACCESS_KEY: &str = "AWS_SECRET_ACCESS_KEY";
/// The session token secret of temporary keys.
pub const SESSION_TOKEN: &str = "AWS_SESSION_TOKEN";
/// The ECS/EKS container credentials path (on [`CONTAINER_HOST`]).
pub const CONTAINER_RELATIVE_URI: &str = "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI";
/// A full container credentials URL.
pub const CONTAINER_FULL_URI: &str = "AWS_CONTAINER_CREDENTIALS_FULL_URI";
/// The authorization token of [`CONTAINER_FULL_URI`].
pub const CONTAINER_TOKEN: &str = "AWS_CONTAINER_AUTHORIZATION_TOKEN";

/// Every name the chain reads from the secrets store (imported from the
/// launch environment like provider keys).
pub const SECRET_NAMES: &[&str] = &[
    ACCESS_KEY_ID,
    SECRET_ACCESS_KEY,
    SESSION_TOKEN,
    CONTAINER_RELATIVE_URI,
    CONTAINER_FULL_URI,
    CONTAINER_TOKEN,
];

/// The ECS container credentials host.
pub const CONTAINER_HOST: &str = "http://169.254.170.2";
/// The EC2 instance metadata service.
pub const IMDS: &str = "http://169.254.169.254";
/// How long a metadata endpoint may take: on a machine without one the
/// chain must give up quickly.
const METADATA_TIMEOUT: Duration = Duration::from_secs(1);

/// Which link of the chain supplied the credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The environment secrets.
    Environment,
    /// Static keys of a shared profile.
    Profile(String),
    /// An IAM Identity Center sign-in of a shared profile.
    Sso(String),
    /// The container credentials endpoint.
    Container,
    /// The EC2 instance role.
    InstanceRole,
}

impl Source {
    /// How doctor and traces name it.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Environment => format!("environment secrets ({ACCESS_KEY_ID})"),
            Self::Profile(name) => format!("shared profile '{name}'"),
            Self::Sso(name) => format!("SSO sign-in of profile '{name}'"),
            Self::Container => "container credentials".to_owned(),
            Self::InstanceRole => "the EC2 instance role".to_owned(),
        }
    }
}

/// The metadata endpoints the chain asks (overridable for tests).
#[derive(Debug, Clone)]
pub struct Endpoints {
    /// IMDS base URL; `None` skips the instance role.
    pub imds: Option<String>,
    /// Host of relative container credential paths.
    pub container_host: String,
    /// IAM Identity Center portal base URL; `None` is the region's
    /// `https://portal.sso.<region>.amazonaws.com`.
    pub sso_portal: Option<String>,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            imds: Some(IMDS.to_owned()),
            container_host: CONTAINER_HOST.to_owned(),
            sso_portal: None,
        }
    }
}

/// Finds AWS credentials.
#[derive(Debug, Clone)]
pub struct Chain {
    client: reqwest::Client,
    user_home: Option<PathBuf>,
    endpoints: Endpoints,
}

impl Chain {
    /// A chain reading profiles under `user_home` and asking endpoints
    /// over `client`.
    #[must_use]
    pub fn new(client: reqwest::Client, user_home: Option<PathBuf>) -> Self {
        Self {
            client,
            user_home,
            endpoints: Endpoints::default(),
        }
    }

    /// The chain asking `endpoints` instead of AWS's.
    #[must_use]
    pub fn with_endpoints(mut self, endpoints: Endpoints) -> Self {
        self.endpoints = endpoints;
        self
    }

    /// The first credentials the chain finds for `provider`.
    ///
    /// # Errors
    ///
    /// `provider.missing_credential` naming the chain when no link has
    /// credentials, or naming the link that failed (an expired SSO
    /// sign-in, a refused exchange).
    pub async fn keys(
        &self,
        provider: &Provider,
        secrets: &dyn SecretSource,
    ) -> Result<(Keys, Source), Error> {
        if let Some(keys) = env_keys(secrets)? {
            return Ok((keys, Source::Environment));
        }
        let name = profile_name(provider);
        if let Some(home) = &self.user_home
            && let Some((settings, session)) = profile::load(home, &name)
        {
            if let Some(keys) = static_keys(&settings) {
                return Ok((keys, Source::Profile(name)));
            }
            if let Some(sso) = profile::sso(home, &settings, session.as_ref()) {
                let keys = self.sso(provider, &name, &sso).await?;
                return Ok((keys, Source::Sso(name)));
            }
        }
        if let Some(keys) = self.container(provider, secrets).await? {
            return Ok((keys, Source::Container));
        }
        if let Some(keys) = self.instance().await {
            return Ok((keys, Source::InstanceRole));
        }
        Err(no_credentials(provider.id(), &name))
    }

    /// Exchanges the cached SSO access token for role credentials.
    async fn sso(&self, provider: &Provider, name: &str, sso: &Sso) -> Result<Keys, Error> {
        let login = format!("aws sso login --profile {name}");
        let token = match profile::sso_token(sso, Timestamp::now()) {
            Some((token, true)) => token,
            Some((_, false)) => {
                return Err(missing(
                    provider.id(),
                    &format!("the SSO sign-in of AWS profile '{name}' expired"),
                    &login,
                ));
            }
            None => {
                return Err(missing(
                    provider.id(),
                    &format!("AWS profile '{name}' uses SSO but has no cached sign-in"),
                    &login,
                ));
            }
        };
        let portal = self
            .endpoints
            .sso_portal
            .clone()
            .unwrap_or_else(|| format!("https://portal.sso.{}.amazonaws.com", sso.region));
        let response = self
            .client
            .get(format!(
                "{portal}/federation/credentials?role_name={}&account_id={}",
                encode(&sso.role_name),
                encode(&sso.account_id)
            ))
            .header("x-amz-sso_bearer_token", token)
            .send()
            .await;
        let refused = |why: String| {
            missing(
                provider.id(),
                &format!("the SSO role credentials of profile '{name}' were refused: {why}"),
                &login,
            )
        };
        let body = json_of(response).await.map_err(refused)?;
        let role = &body["roleCredentials"];
        let expires_at = role["expiration"]
            .as_i64()
            .and_then(|ms| Timestamp::from_millisecond(ms).ok());
        keys_of(
            role,
            "accessKeyId",
            "secretAccessKey",
            "sessionToken",
            expires_at,
        )
        .ok_or_else(|| refused("the answer holds no credentials".to_owned()))
    }

    /// Credentials from the container endpoint the secrets name.
    async fn container(
        &self,
        provider: &Provider,
        secrets: &dyn SecretSource,
    ) -> Result<Option<Keys>, Error> {
        let url = match (
            secrets.secret(CONTAINER_RELATIVE_URI)?,
            secrets.secret(CONTAINER_FULL_URI)?,
        ) {
            (Some(path), _) => format!("{}{}", self.endpoints.container_host, path.expose()),
            (None, Some(full)) => full.expose().to_owned(),
            (None, None) => return Ok(None),
        };
        let mut request = self.client.get(url).timeout(METADATA_TIMEOUT * 5);
        if let Some(token) = secrets.secret(CONTAINER_TOKEN)? {
            request = request.header("authorization", token.expose());
        }
        let body = json_of(request.send().await).await.map_err(|why| {
            missing(
                provider.id(),
                &format!("the container credentials endpoint failed: {why}"),
                "check the task role of the container",
            )
        })?;
        Ok(metadata_keys(&body))
    }

    /// The instance role's credentials through `IMDSv2`; `None` when there
    /// is no metadata service.
    async fn instance(&self) -> Option<Keys> {
        let imds = self.endpoints.imds.as_deref()?;
        let token = self
            .client
            .put(format!("{imds}/latest/api/token"))
            .header("x-aws-ec2-metadata-token-ttl-seconds", "21600")
            .timeout(METADATA_TIMEOUT)
            .send()
            .await
            .ok()
            .filter(|r| r.status().is_success())?
            .text()
            .await
            .ok()?;
        let get = |path: String| {
            self.client
                .get(format!(
                    "{imds}/latest/meta-data/iam/security-credentials/{path}"
                ))
                .header("x-aws-ec2-metadata-token", token.clone())
                .timeout(METADATA_TIMEOUT)
                .send()
        };
        let roles = get(String::new())
            .await
            .ok()
            .filter(|r| r.status().is_success())?
            .text()
            .await
            .ok()?;
        let role = roles.lines().next()?.trim().to_owned();
        let body = json_of(get(role).await).await.ok()?;
        metadata_keys(&body)
    }
}

/// The profile the chain reads.
#[must_use]
pub fn profile_name(provider: &Provider) -> String {
    provider
        .settings()
        .aws_profile
        .clone()
        .unwrap_or_else(|| "default".to_owned())
}

fn env_keys(secrets: &dyn SecretSource) -> Result<Option<Keys>, Error> {
    let (Some(id), Some(secret)) = (
        secrets.secret(ACCESS_KEY_ID)?,
        secrets.secret(SECRET_ACCESS_KEY)?,
    ) else {
        return Ok(None);
    };
    Ok(Some(Keys {
        access_key_id: id.expose().to_owned(),
        secret_access_key: secret,
        session_token: secrets.secret(SESSION_TOKEN)?,
        expires_at: None,
    }))
}

fn static_keys(settings: &profile::Settings) -> Option<Keys> {
    Some(Keys {
        access_key_id: settings.get("aws_access_key_id")?.clone(),
        secret_access_key: SecretString::new(settings.get("aws_secret_access_key")?.as_str()),
        session_token: settings
            .get("aws_session_token")
            .map(|t| SecretString::new(t.as_str())),
        expires_at: None,
    })
}

/// The `AccessKeyId`/`SecretAccessKey`/`Token`/`Expiration` shape of the
/// container and instance endpoints.
fn metadata_keys(body: &Value) -> Option<Keys> {
    let expires_at = body["Expiration"]
        .as_str()
        .and_then(|at| at.parse::<Timestamp>().ok());
    keys_of(body, "AccessKeyId", "SecretAccessKey", "Token", expires_at)
}

fn keys_of(
    body: &Value,
    id: &str,
    secret: &str,
    token: &str,
    expires_at: Option<Timestamp>,
) -> Option<Keys> {
    Some(Keys {
        access_key_id: body[id].as_str()?.to_owned(),
        secret_access_key: SecretString::new(body[secret].as_str()?),
        session_token: body[token].as_str().map(SecretString::new),
        expires_at,
    })
}

async fn json_of(response: reqwest::Result<reqwest::Response>) -> Result<Value, String> {
    let response = response.map_err(|e| e.to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    let text = response.text().await.map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

fn encode(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, super::PATH_SEGMENT).to_string()
}

fn missing(provider: &str, message: &str, hint: &str) -> Error {
    Error::new(
        &codes::PROVIDER_MISSING_CREDENTIAL,
        format!("{provider}: {message}"),
    )
    .with_hint(hint.to_owned())
}

/// No link of the chain had credentials.
fn no_credentials(provider: &str, profile: &str) -> Error {
    missing(
        provider,
        &format!(
            "no AWS credentials found in the AWS credential chain (checked: environment secrets {ACCESS_KEY_ID}/{SECRET_ACCESS_KEY}, shared profile '{profile}' in ~/.aws, container credentials, the EC2 instance role)"
        ),
        &format!(
            "prometheus secrets set {ACCESS_KEY_ID} and {SECRET_ACCESS_KEY}, or run aws sso login and set providers.{provider}.aws_profile"
        ),
    )
}

/// What the chain would use, found without any network request (the
/// container endpoint and instance role are only named).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Located {
    /// A link with usable credentials.
    Found(Source),
    /// The profile's SSO sign-in expired or is missing from the cache.
    SsoLoginNeeded(String),
    /// Nothing local; only the instance role could still answer.
    Nothing(String),
}

/// [`Located`] for `provider` (`prometheus doctor`).
///
/// # Errors
///
/// Secrets store errors.
pub fn locate(
    provider: &Provider,
    secrets: &dyn SecretSource,
    user_home: Option<&std::path::Path>,
) -> Result<Located, Error> {
    if env_keys(secrets)?.is_some() {
        return Ok(Located::Found(Source::Environment));
    }
    let name = profile_name(provider);
    if let Some(home) = user_home
        && let Some((settings, session)) = profile::load(home, &name)
    {
        if static_keys(&settings).is_some() {
            return Ok(Located::Found(Source::Profile(name)));
        }
        if let Some(sso) = profile::sso(home, &settings, session.as_ref()) {
            return Ok(match profile::sso_token(&sso, Timestamp::now()) {
                Some((_, true)) => Located::Found(Source::Sso(name)),
                _ => Located::SsoLoginNeeded(name),
            });
        }
    }
    if secrets.secret(CONTAINER_RELATIVE_URI)?.is_some()
        || secrets.secret(CONTAINER_FULL_URI)?.is_some()
    {
        return Ok(Located::Found(Source::Container));
    }
    Ok(Located::Nothing(name))
}
