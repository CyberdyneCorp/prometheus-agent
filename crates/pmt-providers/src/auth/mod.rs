//! Provider credentials and subscription sign-in (spec: model-providers,
//! Credentials are bound to their authoritative hosts; Credential
//! management commands; Subscription and OAuth sign-in registry;
//! Device-code and browser PKCE sign-in flows; Anthropic Claude Max
//! sign-in; OpenAI Codex ChatGPT plan sign-in).
//!
//! [`spec`] holds each provider's sign-in endpoints, [`device`] and
//! [`browser`] the two reusable flows, [`credentials`] where API keys and
//! sign-ins live in the profile's secrets store (and, as
//! [`CredentialExtras`](crate::resolve::CredentialExtras), what requests
//! use), [`codex`] the Codex CLI import, [`borrowed`] other agent CLIs'
//! logins, [`refresh`] token refresh under a cross-process lock and
//! [`Accounts`] the per-request credential work. [`sign_in`] runs a provider's
//! sign-in: its plan notes first, then the chosen flow, falling back from a
//! busy fixed browser port to the device code.

mod accounts;
pub mod borrowed;
pub mod browser;
pub mod codex;
pub mod credentials;
pub mod device;
mod extras;
pub mod jwt;
pub mod loopback;
pub mod pkce;
pub mod refresh;
pub mod spec;
pub mod token;

use pmt_config::schema::LoginFlow;
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;

use crate::registry::Provider;
pub use accounts::Accounts;
pub use credentials::{
    Credentials, DEFAULT_LABEL, Obtained, Status, Subscription, reauth_required,
};
use token::TokenSet;

/// Where a sign-in talks to the user. It never receives a token, code or
/// verifier; the authorization URL (with its `state`) is shown only to
/// the user, never logged.
pub trait SignInUi: Send + Sync {
    /// Shows one message.
    fn say(&self, text: &str);

    /// Opens `url` in a browser; `false` when it could not.
    fn open_browser(&self, _url: &str) -> bool {
        false
    }
}

/// The flow a user asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowChoice {
    /// `providers.<id>.login_flow`, else the provider's first flow.
    Default,
    /// `--browser`.
    Browser,
    /// `--device`.
    Device,
}

/// A completed sign-in.
#[derive(Debug)]
pub struct Completed {
    /// The tokens.
    pub tokens: TokenSet,
    /// What to record with them.
    pub obtained: Obtained,
}

/// Runs `provider`'s sign-in: prints its plan notes, then the chosen flow;
/// a browser sign-in whose fixed redirect port is busy continues with the
/// device code when the provider offers one.
///
/// # Errors
///
/// `auth.unsupported` (no sign-in or flow), `auth.signin_unavailable`,
/// `auth.insecure_endpoint`, `auth.host_not_allowed`, the flows' errors.
pub async fn sign_in(
    client: &reqwest::Client,
    provider: &Provider,
    choice: FlowChoice,
    ui: &dyn SignInUi,
    place: browser::Place,
) -> Result<Completed, Error> {
    let flows = provider.login_flows();
    let flow = pick(provider.id(), &flows, choice)?;
    let spec = spec::for_provider(provider)?;
    spec.check()?;
    for note in provider.plan_notes() {
        ui.say(&format!("note: {note}"));
    }
    let (tokens, source) = match flow {
        LoginFlow::Device => (device::run(client, &spec, ui).await?, "device"),
        LoginFlow::Browser => match browser::bind(&spec).await {
            Ok(listener) => (
                browser::run(client, &spec, &listener, ui, place).await?,
                "browser",
            ),
            Err(busy)
                if busy.code() == &codes::AUTH_PORT_BUSY && flows.contains(&LoginFlow::Device) =>
            {
                ui.say(&format!(
                    "Port {} is in use, so the browser sign-in cannot receive its callback; continuing with the device-code sign-in.",
                    spec.redirect.port
                ));
                (device::run(client, &spec, ui).await?, "device")
            }
            Err(err) => return Err(err),
        },
    };
    let access = tokens.access_token.expose();
    let id_token = tokens
        .id_token
        .as_ref()
        .map(SecretString::expose)
        .unwrap_or_default();
    let account = jwt::account(&[id_token, access]);
    let obtained = Obtained {
        source: source.to_owned(),
        account_id: account.account_id,
        plan: account.plan,
    };
    Ok(Completed { tokens, obtained })
}

fn pick(id: &str, flows: &[LoginFlow], choice: FlowChoice) -> Result<LoginFlow, Error> {
    let wanted = match choice {
        FlowChoice::Default => flows.first().copied(),
        FlowChoice::Browser => Some(LoginFlow::Browser),
        FlowChoice::Device => Some(LoginFlow::Device),
    };
    match wanted {
        Some(flow) if flows.contains(&flow) => Ok(flow),
        Some(flow) => Err(Error::new(
            &codes::AUTH_UNSUPPORTED,
            format!(
                "provider '{id}' offers no {} sign-in",
                match flow {
                    LoginFlow::Browser => "browser",
                    LoginFlow::Device => "device-code",
                }
            ),
        )),
        None => Err(Error::new(
            &codes::AUTH_UNSUPPORTED,
            format!("provider '{id}' has no sign-in"),
        )
        .with_hint(format!(
            "use an API key: prometheus auth add {id} --type api_key"
        ))),
    }
}
