//! `prometheus gateway photon setup|status` (spec: platform-adapters,
//! Photon iMessage adapter). Setup signs in to the Photon dashboard with
//! the OAuth device flow (RFC 8628, client `photon-cli`), finds or creates
//! the profile's project, provisions its Spectrum project secret into the
//! secrets store (`PHOTON_PROJECT_ID`, `PHOTON_PROJECT_SECRET`; the
//! dashboard token is kept as `PHOTON_DASHBOARD_TOKEN` so a re-run reuses
//! it), registers the user's phone number as a Spectrum user, and installs
//! the sidecar's runtime and packages. Status reports what is configured
//! and the iMessage line assigned to each registered number. No token or
//! secret is ever printed.
//!
//! Endpoints default to Photon's (`https://app.photon.codes`,
//! `https://spectrum.photon.codes`); `[gateway.platforms.photon]` options
//! `dashboard_url` and `spectrum_url` point elsewhere, `dashboard_project`
//! names the project (default `Prometheus Agent`).

use std::fmt::Write as _;
use std::time::Duration;

use clap::{Args, Subcommand};
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use serde_json::{Value, json};

use crate::context::Context;

const PLATFORM: &str = "photon";
const CLIENT_ID: &str = "photon-cli";
const TOKEN: &str = "PHOTON_DASHBOARD_TOKEN";
const PROJECT_ID: &str = "PHOTON_PROJECT_ID";
const PROJECT_SECRET: &str = "PHOTON_PROJECT_SECRET";
const DEFAULT_PROJECT: &str = "Prometheus Agent";

/// `prometheus gateway photon`.
#[derive(Debug, Clone, Args)]
pub struct PhotonArgs {
    /// The subcommand.
    #[command(subcommand)]
    pub command: PhotonCommand,
}

/// `gateway photon` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum PhotonCommand {
    /// Sign in to Photon (device login), provision the project and its
    /// secret, register your phone number and install the sidecar.
    Setup {
        /// Your phone number in E.164 form (`+15551234567`): the number
        /// that texts the agent.
        #[arg(long)]
        phone: String,
        /// Leave installing the sidecar's runtime to the adapter's first
        /// connect (an offline machine, say).
        #[arg(long)]
        no_install: bool,
    },
    /// Print what is configured and each registered number's iMessage line.
    Status,
}

/// The Photon endpoints a profile uses.
struct Endpoints {
    dashboard: String,
    spectrum: String,
    project_name: String,
}

impl Endpoints {
    fn of(scope: &pmt_config::ProfileScope) -> Self {
        let config = scope.config();
        let options = config
            .gateway
            .platforms
            .get(PLATFORM)
            .map(|p| p.options.clone())
            .unwrap_or_default();
        let text = |key: &str, default: &str| {
            options
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or(default)
                .trim_end_matches('/')
                .to_owned()
        };
        Self {
            dashboard: text("dashboard_url", "https://app.photon.codes"),
            spectrum: text("spectrum_url", "https://spectrum.photon.codes"),
            project_name: text("dashboard_project", DEFAULT_PROJECT),
        }
    }
}

fn failed(what: &str, why: impl std::fmt::Display) -> Error {
    Error::new(&codes::NET_REQUEST, format!("Photon {what}: {why}"))
}

/// Whether `phone` is E.164.
fn is_e164(phone: &str) -> bool {
    phone.strip_prefix('+').is_some_and(|digits| {
        (7..=15).contains(&digits.len())
            && !digits.starts_with('0')
            && digits.bytes().all(|b| b.is_ascii_digit())
    })
}

struct Api {
    client: reqwest::Client,
    endpoints: Endpoints,
}

impl Api {
    async fn call(
        &self,
        request: reqwest::RequestBuilder,
        what: &str,
    ) -> Result<(u16, Value), Error> {
        let response = request.send().await.map_err(|e| failed(what, e))?;
        let status = response.status().as_u16();
        let header_token = response
            .headers()
            .get("set-auth-token")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mut body: Value = response.json().await.unwrap_or(Value::Null);
        if let (Some(token), Value::Object(map)) = (header_token, &mut body) {
            map.entry("set-auth-token").or_insert(json!(token));
        }
        Ok((status, body))
    }

    async fn dashboard(
        &self,
        method: reqwest::Method,
        path: &str,
        token: &str,
        body: Option<&Value>,
    ) -> Result<Value, Error> {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.endpoints.dashboard))
            .bearer_auth(token);
        if let Some(body) = body {
            request = request.json(body);
        }
        let (status, answer) = self.call(request, path).await?;
        if (200..300).contains(&status) && answer.get("error").is_none() {
            Ok(answer)
        } else {
            Err(failed(path, format!("HTTP {status}: {}", detail(&answer))))
        }
    }

    async fn spectrum(
        &self,
        method: reqwest::Method,
        project: &str,
        secret: &str,
        body: Option<&Value>,
    ) -> Result<Value, Error> {
        let mut request = self
            .client
            .request(
                method,
                format!("{}/projects/{project}/users/", self.endpoints.spectrum),
            )
            .basic_auth(project, Some(secret));
        if let Some(body) = body {
            request = request.json(body);
        }
        let (status, answer) = self.call(request, "users").await?;
        if (200..300).contains(&status) {
            Ok(answer)
        } else {
            Err(failed(
                "users",
                format!("HTTP {status}: {}", detail(&answer)),
            ))
        }
    }
}

fn detail(answer: &Value) -> String {
    answer["error"]
        .as_str()
        .or(answer["message"].as_str())
        .map_or_else(|| answer.to_string(), str::to_owned)
}

/// A list a Photon API answered, bare or under `data`/`users`/`projects`.
fn list(answer: &Value) -> Vec<Value> {
    ["data", "users", "projects"]
        .iter()
        .find_map(|k| answer[k].as_array())
        .or(answer.as_array())
        .cloned()
        .unwrap_or_default()
}

/// The access token of a device-token answer.
fn token_of(answer: &Value) -> Option<String> {
    [
        &answer["access_token"],
        &answer["data"]["access_token"],
        &answer["accessToken"],
        &answer["set-auth-token"],
    ]
    .iter()
    .find_map(|v| v.as_str().filter(|t| !t.is_empty()))
    .map(str::to_owned)
}

/// The device flow: shows the code, polls until approved.
async fn device_login(api: &Api) -> Result<String, Error> {
    let url = format!("{}/api/auth/device/code", api.endpoints.dashboard);
    let request = api
        .client
        .post(url)
        .json(&json!({"client_id": CLIENT_ID, "scope": "openid profile email"}));
    let (status, code) = api.call(request, "device login").await?;
    if status != 200 {
        return Err(Error::new(
            &codes::AUTH_LOGIN_FAILED,
            format!("Photon device login: HTTP {status}: {}", detail(&code)),
        ));
    }
    let shown = code["verification_uri_complete"]
        .as_str()
        .or(code["verification_uri"].as_str())
        .unwrap_or_default();
    crate::output::print_raw(&format!(
        "Open {shown} and approve the code {}\n",
        code["user_code"].as_str().unwrap_or_default()
    ))?;
    let device = code["device_code"].as_str().unwrap_or_default().to_owned();
    let mut interval = code["interval"].as_u64().unwrap_or(5);
    let deadline = std::time::Instant::now()
        + Duration::from_secs(code["expires_in"].as_u64().unwrap_or(1800));
    let grant = json!({"grant_type": "urn:ietf:params:oauth:grant-type:device_code", "device_code": device, "client_id": CLIENT_ID});
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let request = api
            .client
            .post(format!("{}/api/auth/device/token", api.endpoints.dashboard))
            .json(&grant);
        let (status, answer) = api.call(request, "device login").await?;
        match (status, answer["error"].as_str().unwrap_or_default()) {
            (200, _) => {
                return token_of(&answer).ok_or_else(|| {
                    Error::new(
                        &codes::AUTH_LOGIN_FAILED,
                        "Photon approved the login but sent no token",
                    )
                });
            }
            (429, _) | (400, "slow_down") => interval += 5,
            (400, "authorization_pending") => {}
            (_, why) => {
                return Err(Error::new(
                    &codes::AUTH_LOGIN_FAILED,
                    format!(
                        "Photon device login failed: {}",
                        if why.is_empty() {
                            status.to_string()
                        } else {
                            why.to_owned()
                        }
                    ),
                ));
            }
        }
    }
    Err(Error::new(
        &codes::AUTH_LOGIN_EXPIRED,
        "the Photon device login expired before it was approved",
    ))
}

/// A dashboard token that still works, or a fresh one from the device flow.
async fn token(
    api: &Api,
    scope: &pmt_config::ProfileScope,
    out: &mut String,
) -> Result<String, Error> {
    if let Ok(stored) = scope.secrets().resolve(TOKEN) {
        let token = stored.expose().to_owned();
        if api
            .dashboard(reqwest::Method::GET, "/api/projects", &token, None)
            .await
            .is_ok()
        {
            out.push_str("[1/5] reusing the stored Photon sign-in\n");
            return Ok(token);
        }
    }
    let token = device_login(api).await?;
    store(scope, TOKEN, &token)?;
    out.push_str("[1/5] signed in to Photon\n");
    Ok(token)
}

fn store(scope: &pmt_config::ProfileScope, name: &str, value: &str) -> Result<(), Error> {
    scope.secrets().store().set(name, &SecretString::new(value))
}

/// The project's id, created when the account has none of that name.
async fn project(api: &Api, token: &str, out: &mut String) -> Result<String, Error> {
    let name = api.endpoints.project_name.clone();
    let projects = api
        .dashboard(reqwest::Method::GET, "/api/projects", token, None)
        .await?;
    if let Some(found) = list(&projects).iter().find(|p| p["name"] == name.as_str()) {
        let _ = writeln!(out, "[2/5] using the Photon project '{name}'");
        return found["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| failed("projects", "a project without an id"));
    }
    let body = json!({"name": name, "location": "United States", "template": false, "observability": false});
    let created = api
        .dashboard(reqwest::Method::POST, "/api/projects", token, Some(&body))
        .await?;
    let project = if created["data"].is_object() {
        &created["data"]
    } else {
        &created
    };
    let _ = writeln!(out, "[2/5] created the Photon project '{name}'");
    project["id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| failed("projects", "the new project has no id"))
}

/// The project secret: the stored one when it is this project's and
/// Spectrum accepts it, else a regenerated one (shown only once by Photon).
async fn secret(
    api: &Api,
    scope: &pmt_config::ProfileScope,
    token: &str,
    project: &str,
    out: &mut String,
) -> Result<String, Error> {
    let same = scope
        .secrets()
        .resolve(PROJECT_ID)
        .is_ok_and(|id| id.expose() == project);
    if same && let Ok(stored) = scope.secrets().resolve(PROJECT_SECRET) {
        let secret = stored.expose().to_owned();
        if api
            .spectrum(reqwest::Method::GET, project, &secret, None)
            .await
            .is_ok()
        {
            out.push_str("[3/5] the stored project secret is valid\n");
            return Ok(secret);
        }
    }
    let path = format!("/api/projects/{project}/regenerate-secret");
    let answer = api
        .dashboard(reqwest::Method::POST, &path, token, Some(&json!({})))
        .await?;
    let secret = answer["projectSecret"]
        .as_str()
        .or(answer["data"]["projectSecret"].as_str())
        .ok_or_else(|| failed("regenerate-secret", "no projectSecret in the answer"))?
        .to_owned();
    store(scope, PROJECT_ID, project)?;
    store(scope, PROJECT_SECRET, &secret)?;
    out.push_str("[3/5] stored a new project secret (restart the gateway if Photon was running)\n");
    Ok(secret)
}

fn digits(phone: &str) -> String {
    phone
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '+')
        .collect()
}

/// Registers `phone` unless a Spectrum user has it.
async fn register(
    api: &Api,
    project: &str,
    secret: &str,
    phone: &str,
    out: &mut String,
) -> Result<Value, Error> {
    let users = api
        .spectrum(reqwest::Method::GET, project, secret, None)
        .await?;
    if let Some(user) = list(&users)
        .into_iter()
        .find(|u| digits(u["phoneNumber"].as_str().unwrap_or_default()) == phone)
    {
        out.push_str("[4/5] the phone number is already registered\n");
        return Ok(user);
    }
    let body = json!({"type": "shared", "phoneNumber": phone});
    let created = api
        .spectrum(reqwest::Method::POST, project, secret, Some(&body))
        .await?;
    out.push_str("[4/5] registered the phone number\n");
    Ok(created
        .get("user")
        .or(created.get("data"))
        .cloned()
        .unwrap_or(created))
}

fn runtime() -> Result<tokio::runtime::Runtime, Error> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::new(&codes::OUTPUT_IO, format!("cannot start the runtime: {e}")))
}

fn api(scope: &pmt_config::ProfileScope) -> Result<Api, Error> {
    let client = pmt_net::http::client_builder(&pmt_net::OutboundPolicy::default())?
        .build()
        .map_err(|e| Error::new(&codes::NET_CLIENT_INIT, e.to_string()))?;
    Ok(Api {
        client,
        endpoints: Endpoints::of(scope),
    })
}

async fn setup(
    scope: &pmt_config::ProfileScope,
    phone: &str,
    install: bool,
) -> Result<String, Error> {
    let api = api(scope)?;
    let mut out = String::new();
    let token = token(&api, scope, &mut out).await?;
    let project = project(&api, &token, &mut out).await?;
    let secret = secret(&api, scope, &token, &project, &mut out).await?;
    let user = register(&api, &project, &secret, phone, &mut out).await?;
    if install {
        crate::plugins_host::adapter::prepare_sidecar(scope, PLATFORM).await?;
        out.push_str("[5/5] installed the sidecar\n");
    } else {
        out.push_str("[5/5] the sidecar installs when the adapter first connects\n");
    }
    match user["assignedPhoneNumber"].as_str() {
        Some(line) => {
            let _ = writeln!(out, "Text {line} from {phone} to reach your agent.");
        }
        None => out.push_str("Photon has not assigned an iMessage line yet; run `prometheus gateway photon status` later.\n"),
    }
    Ok(out)
}

async fn status(scope: &pmt_config::ProfileScope) -> Result<String, Error> {
    let api = api(scope)?;
    let has = |name: &str| scope.secrets().resolve(name).is_ok();
    let mut out = String::new();
    let enabled = scope
        .config()
        .gateway
        .platforms
        .get(PLATFORM)
        .is_some_and(|p| p.enabled);
    let _ = writeln!(
        out,
        "adapter enabled: {}",
        if enabled { "yes" } else { "no" }
    );
    let _ = writeln!(
        out,
        "dashboard sign-in: {}",
        if has(TOKEN) { "stored" } else { "missing" }
    );
    let (Ok(project), Ok(secret)) = (
        scope.secrets().resolve(PROJECT_ID),
        scope.secrets().resolve(PROJECT_SECRET),
    ) else {
        out.push_str(
            "project credentials: missing (run `prometheus gateway photon setup --phone <E164>`)\n",
        );
        return Ok(out);
    };
    let _ = writeln!(out, "project: {}", project.expose());
    match api
        .spectrum(
            reqwest::Method::GET,
            project.expose(),
            secret.expose(),
            None,
        )
        .await
    {
        Ok(users) => {
            for user in list(&users) {
                let line = user["assignedPhoneNumber"]
                    .as_str()
                    .unwrap_or("no line assigned yet");
                let _ = writeln!(
                    out,
                    "number {}: texts {line}",
                    user["phoneNumber"].as_str().unwrap_or("?")
                );
            }
        }
        Err(e) => {
            let _ = writeln!(out, "spectrum: unreachable or refused ({})", e.message());
        }
    }
    Ok(out)
}

/// Runs `gateway photon <command>`.
///
/// # Errors
///
/// A bad phone number, a refused or expired login, or a Photon API error.
pub(super) fn run(args: &PhotonArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let text = match &args.command {
        PhotonCommand::Setup { phone, no_install } => {
            if !is_e164(phone) {
                return Err(Error::new(
                    &codes::CLI_USAGE,
                    format!("'{phone}' is not an E.164 phone number (+15551234567)"),
                ));
            }
            runtime()?.block_on(setup(&scope, phone, !no_install))?
        }
        PhotonCommand::Status => runtime()?.block_on(status(&scope))?,
    };
    crate::output::print_raw(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phones_are_e164_and_tokens_come_in_any_documented_field() {
        assert!(is_e164("+15551234567"));
        assert!(!is_e164("15551234567") && !is_e164("+0555123") && !is_e164("+1555"));
        assert_eq!(
            token_of(&json!({"data": {"access_token": "t"}})).as_deref(),
            Some("t")
        );
        assert_eq!(
            token_of(&json!({"set-auth-token": "h"})).as_deref(),
            Some("h")
        );
        assert_eq!(token_of(&json!({})), None);
    }
}
