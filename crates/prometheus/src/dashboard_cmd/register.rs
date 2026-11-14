//! `prometheus dashboard register` (spec: web-dashboard, Registering a
//! dashboard with the account portal): with the profile's portal sign-in,
//! registers a self-hosted OAuth client for this dashboard at the portal
//! (`POST <issuer>/api/oauth/clients`) and saves the returned client id to
//! `dashboard.auth.portal.client_id` through a validated `daemon.toml`
//! edit. Without a portal sign-in it exits 78; a redirect URI that is not
//! `https` (loopback aside) exits 2.

use std::time::Duration;

use pmt_config::daemon_dashboard::check_public_url;
use pmt_core::error::{Error, codes};
use pmt_providers::auth::gateway::GATEWAY_PROVIDER;
use serde_json::{Value, json};

use crate::context::Context;
use crate::output::{document, print_raw};

/// The config key the client id is saved to.
const CLIENT_ID_KEY: &str = "dashboard.auth.portal.client_id";

/// `dashboard register` arguments.
#[derive(Debug, Clone, clap::Args)]
pub struct RegisterArgs {
    /// The label the portal shows for this dashboard.
    #[arg(long, default_value = "Prometheus dashboard")]
    pub name: String,
    /// The OAuth callback (`https://dash.example.com/auth/callback`);
    /// default: `<dashboard.public_url>/auth/callback`.
    #[arg(long, value_name = "URL")]
    pub redirect_uri: Option<String>,
}

fn usage(message: String) -> Error {
    Error::new(&codes::CLI_USAGE, message)
}

/// The callback to register: `--redirect-uri`, else from
/// `dashboard.public_url`.
fn redirect_uri(args: &RegisterArgs, config: &pmt_config::DaemonConfig) -> Result<String, Error> {
    let uri = match (&args.redirect_uri, &config.dashboard.public_url) {
        (Some(uri), _) => uri.clone(),
        (None, Some(public)) => format!("{}/auth/callback", public.trim_end_matches('/')),
        (None, None) => {
            return Err(usage(
                "pass --redirect-uri https://<host>/auth/callback or set dashboard.public_url"
                    .to_owned(),
            ));
        }
    };
    check_public_url("--redirect-uri", &uri).map_err(usage)?;
    Ok(uri)
}

/// Runs `dashboard register`.
///
/// # Errors
///
/// `cli.usage` (2) for a bad redirect URI, `dashboard.portal_signin_required`
/// (78) without a portal sign-in, `dashboard.register_failed` (69) when
/// the portal refuses, configuration errors.
pub(super) fn run(args: &RegisterArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let home = ctx.home()?;
    let config = pmt_config::DaemonConfig::load(home.root())?;
    let redirect = redirect_uri(args, &config)?;
    let scope = ctx.scope()?;
    let credentials = pmt_providers::auth::Credentials::new(scope.secrets().store().clone());
    let token = credentials
        .subscriptions()?
        .into_iter()
        .find(|s| s.provider == GATEWAY_PROVIDER)
        .map(|sub| credentials.access_token(&sub))
        .transpose()?
        .flatten()
        .ok_or_else(|| {
            Error::new(
                &codes::DASHBOARD_PORTAL_SIGNIN,
                "sign in to the portal first: prometheus auth login portal",
            )
        })?;
    let issuer = config.dashboard.auth.portal.issuer_url().to_owned();
    let body = json!({"name": args.name, "redirect_uris": [redirect], "kind": "dashboard"});
    let answer = crate::model_cmd::block_on(post(&issuer, token.expose(), &body))??;
    let client_id = answer["client_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| failed("the portal answered without a client_id".to_owned()))?;
    pmt_config::daemon_edit::set_string(home.root(), CLIENT_ID_KEY, client_id)?;
    let name = answer["name"].as_str().unwrap_or(&args.name);
    if ctx.json {
        return document(
            "dashboard register",
            &json!({"name": name, "client_id": client_id, "redirect_uri": redirect}),
        );
    }
    print_raw(&format!(
        "Registered '{name}' with the account portal; saved {CLIENT_ID_KEY} = {client_id}.\n\
         Set dashboard.auth.mode = \"portal\" to sign in with it (restart the daemon).\n"
    ))
}

fn failed(message: String) -> Error {
    Error::new(&codes::DASHBOARD_REGISTER_FAILED, message)
}

async fn post(issuer: &str, token: &str, body: &Value) -> Result<Value, Error> {
    let client = pmt_net::http::client_builder(&pmt_net::http::OutboundPolicy::default())?
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| failed(format!("no HTTP client: {e}")))?;
    let url = format!("{issuer}/api/oauth/clients");
    let response = client
        .post(&url)
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .map_err(|e| failed(format!("the account portal is unreachable at {url}: {e}")))?;
    let status = response.status();
    let bytes = response.bytes().await.unwrap_or_default();
    if !status.is_success() {
        return Err(failed(format!(
            "the account portal refused the registration ({status})"
        )));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| failed("the account portal did not answer JSON".to_owned()))
}
