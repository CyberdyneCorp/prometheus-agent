//! `prometheus gateway proxy start|status|providers` (spec: api-server,
//! Subscription proxy): a local OpenAI-compatible passthrough that lends
//! the profile's subscription sign-in to other tools, with no agent loop
//! and no tools. It binds loopback unless `--allow-lan`, in which case
//! clients must present `PMT_PROXY_CLIENT_KEY` (generated on first use).

mod host;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use clap::Subcommand;
use pmt_api::subscription_proxy::{self, DEFAULT_PORT, Proxy, Upstream};
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_providers::auth::credentials::{Credentials, Status, Subscription};

use crate::context::Context;
use crate::output::{json, print};

/// The secret LAN clients present.
pub(crate) const CLIENT_KEY_SECRET: &str = "PMT_PROXY_CLIENT_KEY";

/// The providers the proxy forwards to: the name on the command line, the
/// provider id and the paths forwarded.
const PROVIDERS: &[(&str, &str, &[&str])] = &[
    (
        "nous",
        "nous",
        &[
            "/v1/chat/completions",
            "/v1/completions",
            "/v1/embeddings",
            "/v1/models",
        ],
    ),
    (
        "xai",
        "xai-oauth",
        &["/v1/chat/completions", "/v1/responses", "/v1/models"],
    ),
];

/// `prometheus gateway proxy` subcommands.
#[derive(Debug, Subcommand)]
pub enum ProxyCommand {
    /// Run the subscription proxy in the foreground (Ctrl-C stops it).
    Start {
        /// The provider whose sign-in it lends: `nous` or `xai`.
        #[arg(long, default_value = "nous")]
        provider: String,
        /// The address to listen on.
        #[arg(long, default_value = "127.0.0.1")]
        host: IpAddr,
        /// The port.
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
        /// Listen beyond loopback; clients then need `PMT_PROXY_CLIENT_KEY`.
        #[arg(long)]
        allow_lan: bool,
    },
    /// Each provider's readiness.
    Status,
    /// The providers the proxy can forward to and their paths.
    Providers,
}

/// Runs one `gateway proxy` subcommand.
///
/// # Errors
///
/// `proxy.not_logged_in` (exit 78), `proxy.lan_refused`, `cli.usage`,
/// bind and secrets errors.
pub fn run(command: &ProxyCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    match command {
        ProxyCommand::Status => {
            let rows: Vec<(String, String)> = PROVIDERS
                .iter()
                .map(|(name, id, _)| ((*name).to_owned(), readiness(&scope, id)))
                .collect();
            if ctx.json {
                let map: serde_json::Map<String, serde_json::Value> =
                    rows.into_iter().map(|(n, r)| (n, r.into())).collect();
                return json(&map);
            }
            let text: Vec<String> = rows.iter().map(|(n, r)| format!("{n}: {r}")).collect();
            print(&text.join("\n"))
        }
        ProxyCommand::Providers => {
            if ctx.json {
                let map: serde_json::Map<String, serde_json::Value> = PROVIDERS
                    .iter()
                    .map(|(name, id, paths)| {
                        (
                            (*name).to_owned(),
                            serde_json::json!({"provider": id, "paths": paths}),
                        )
                    })
                    .collect();
                return json(&map);
            }
            let text: Vec<String> = PROVIDERS
                .iter()
                .map(|(name, id, paths)| format!("{name} ({id}): {}", paths.join(", ")))
                .collect();
            print(&text.join("\n"))
        }
        ProxyCommand::Start {
            provider,
            host,
            port,
            allow_lan,
        } => start(
            &scope,
            ctx,
            provider,
            SocketAddr::new(*host, *port),
            *allow_lan,
        ),
    }
}

/// `ready (expires <time>)`, `not logged in` or `credentials need
/// attention`.
fn readiness(scope: &ProfileScope, provider: &str) -> String {
    match usable(scope, provider) {
        Ok(Some(sub)) => match sub.expires_at {
            Some(at) => format!("ready (expires {at})"),
            None => "ready".to_owned(),
        },
        Ok(None) => "not logged in".to_owned(),
        Err(()) => "credentials need attention".to_owned(),
    }
}

/// The usable sign-in of `provider`; `Err` when every sign-in it has is
/// dead.
fn usable(scope: &ProfileScope, provider: &str) -> Result<Option<Subscription>, ()> {
    let credentials = Credentials::new(scope.secrets().store().clone());
    let mine: Vec<Subscription> = credentials
        .subscriptions()
        .map_err(|_| ())?
        .into_iter()
        .filter(|s| s.provider == provider)
        .collect();
    match mine.iter().find(|s| s.status == Status::Ok) {
        Some(sub) => Ok(Some(sub.clone())),
        None if mine.is_empty() => Ok(None),
        None => Err(()),
    }
}

fn start(
    scope: &ProfileScope,
    ctx: &Context<'_>,
    name: &str,
    listen: SocketAddr,
    allow_lan: bool,
) -> Result<u8, Error> {
    let Some((_, id, paths)) = PROVIDERS
        .iter()
        .find(|(n, id, _)| *n == name || *id == name)
    else {
        let names: Vec<&str> = PROVIDERS.iter().map(|(n, _, _)| *n).collect();
        return Err(Error::new(
            &codes::CLI_USAGE,
            format!("the proxy forwards to {}, not '{name}'", names.join(" or ")),
        ));
    };
    let Ok(Some(sub)) = usable(scope, id) else {
        return Err(Error::new(
            &codes::PROXY_NOT_LOGGED_IN,
            format!("{name}: {}", readiness(scope, id)),
        )
        .with_hint(format!("prometheus auth add {id}")));
    };
    if !listen.ip().is_loopback() && !allow_lan {
        return Err(Error::new(
            &codes::PROXY_LAN_REFUSED,
            format!("refusing to listen on {listen}: it is not a loopback address"),
        )
        .with_hint("add --allow-lan; LAN clients then present PMT_PROXY_CLIENT_KEY"));
    }
    let client_key = (!listen.ip().is_loopback())
        .then(|| client_key(scope))
        .transpose()?;
    let profile_host = host::ProfileHost::open(scope, ctx, id, &sub.label)?;
    let upstream = Upstream {
        provider: (*id).to_owned(),
        base_url: profile_host.base_url().trim_end_matches('/').to_owned(),
        allowed: paths.iter().map(|p| (*p).to_owned()).collect(),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::new(&codes::TASK_PANICKED, format!("no async runtime: {e}")))?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(listen).await.map_err(|e| {
            Error::new(
                &codes::NET_REQUEST,
                format!("cannot listen on {listen}: {e}"),
            )
        })?;
        let bound = listener.local_addr().unwrap_or(listen);
        eprintln!("subscription proxy for {name} on http://{bound}/v1 (Ctrl-C stops it)");
        let client = crate::model_cmd::client(scope.config())?;
        let proxy = Arc::new(Proxy {
            upstream,
            listen: bound,
            client_key,
            host: Arc::new(profile_host),
            client,
        });
        let stop = pmt_core::cancel::CancelToken::new();
        let serving = subscription_proxy::serve(listener, proxy, stop.clone());
        tokio::select! {
            () = serving => {}
            _ = tokio::signal::ctrl_c() => stop.cancel(),
        }
        Ok::<_, Error>(())
    })?;
    Ok(0)
}

/// `PMT_PROXY_CLIENT_KEY`, generated and stored on first use.
fn client_key(scope: &ProfileScope) -> Result<String, Error> {
    let secrets = scope.secrets().store();
    if let Some(key) = secrets.get(CLIENT_KEY_SECRET)? {
        return Ok(key.expose().to_owned());
    }
    let key = pmt_providers::auth::pkce::random_token(32)?;
    secrets.set(CLIENT_KEY_SECRET, &key)?;
    eprintln!(
        "generated the proxy client key and stored it as {CLIENT_KEY_SECRET} (show it with: prometheus secrets get {CLIENT_KEY_SECRET} --reveal)"
    );
    Ok(key.expose().to_owned())
}
