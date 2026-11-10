//! Subscription tool gateway routing (spec: model-providers, Subscription
//! tool gateway routing).
//!
//! Each tool category has one selection key owned by the tool's
//! capability. When it names a subscription provider (`nous`) the
//! category's calls go through that provider's tool gateway with the
//! sign-in's token and bill the subscription. A stored selection is always
//! honoured, whatever secrets exist: a selected vendor without its key
//! fails naming the selection and never falls over to the gateway. Only a
//! category that was never configured is detected (its vendor key, else a
//! signed-in subscription).

use pmt_config::Config;
use pmt_config::provider_urls::check_url;
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use serde::Serialize;

use crate::resolve::{CredentialExtras, SecretSource};

/// The subscription provider with a tool gateway.
pub const GATEWAY_PROVIDER: &str = "nous";

/// The portal where the subscription is managed.
pub const PORTAL_URL: &str = "https://portal.nousresearch.com";

/// A tool category the gateway serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// Web search.
    WebSearch,
    /// Web page extraction.
    WebExtract,
    /// Image generation.
    ImageGeneration,
    /// Video generation.
    VideoGeneration,
    /// Text to speech.
    Tts,
    /// Speech to text.
    Stt,
    /// Cloud browser.
    Browser,
    /// Managed remote sandbox (terminal backend).
    Sandbox,
}

impl Category {
    /// Every category.
    pub const ALL: [Self; 8] = [
        Self::WebSearch,
        Self::WebExtract,
        Self::ImageGeneration,
        Self::VideoGeneration,
        Self::Tts,
        Self::Stt,
        Self::Browser,
        Self::Sandbox,
    ];

    /// The category's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::WebSearch => "web_search",
            Self::WebExtract => "web_extract",
            Self::ImageGeneration => "image_generation",
            Self::VideoGeneration => "video_generation",
            Self::Tts => "tts",
            Self::Stt => "stt",
            Self::Browser => "browser",
            Self::Sandbox => "sandbox",
        }
    }

    /// Parses a category name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name)
    }

    /// The config key holding the category's selection.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::WebSearch => "web.search_backend",
            Self::WebExtract => "web.extract_backend",
            Self::ImageGeneration => "image_gen.provider",
            Self::VideoGeneration => "video_gen.provider",
            Self::Tts => "tts.provider",
            Self::Stt => "stt.provider",
            Self::Browser => "browser.provider",
            Self::Sandbox => "terminal.backend",
        }
    }

    /// The gateway label (`<label>-gateway.<domain>`).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::WebSearch | Self::WebExtract => "web",
            Self::ImageGeneration => "image",
            Self::VideoGeneration => "video",
            Self::Tts => "tts",
            Self::Stt => "stt",
            Self::Browser => "browser",
            Self::Sandbox => "sandbox",
        }
    }

    /// The stored selection and the key that holds it; `None` when the
    /// category was never configured (`web.backend = "auto"` included).
    #[must_use]
    pub fn selection(self, config: &Config) -> Option<(String, &'static str)> {
        let web = &config.web;
        let shared = || (web.backend != "auto").then(|| (web.backend.clone(), "web.backend"));
        let own = |value: &Option<String>| value.clone().map(|v| (v, self.key()));
        match self {
            Self::WebSearch => own(&web.search_backend).or_else(shared),
            Self::WebExtract => own(&web.extract_backend).or_else(shared),
            Self::ImageGeneration => own(&config.image_gen.provider),
            Self::VideoGeneration => own(&config.video_gen.provider),
            Self::Tts => own(&config.tts.provider),
            Self::Stt => own(&config.stt.provider),
            // The browser tools' own name of the gateway provider.
            Self::Browser => Some((
                match config.browser.provider.as_str() {
                    "subscription" => GATEWAY_PROVIDER.to_owned(),
                    other => other.to_owned(),
                },
                self.key(),
            )),
            // The managed remote sandbox is Modal in managed mode.
            Self::Sandbox => {
                let terminal = &config.terminal;
                let managed = terminal.backend == GATEWAY_PROVIDER
                    || (terminal.backend == "modal"
                        && terminal.modal.mode == pmt_config::schema::ModalMode::Managed);
                let value = if managed {
                    GATEWAY_PROVIDER.to_owned()
                } else {
                    terminal.backend.clone()
                };
                Some((value, self.key()))
            }
        }
    }
}

/// The secret a tool vendor's key lives in; `None` for keyless vendors
/// (local engines, self-hosted backends).
#[must_use]
pub fn vendor_secret(vendor: &str) -> Option<&'static str> {
    Some(match vendor {
        "fal" => "FAL_KEY",
        "openai" => "OPENAI_API_KEY",
        "xai" => "XAI_API_KEY",
        "elevenlabs" => "ELEVENLABS_API_KEY",
        "deepgram" => "DEEPGRAM_API_KEY",
        "brave" => "BRAVE_API_KEY",
        "tavily" => "TAVILY_API_KEY",
        "exa" => "EXA_API_KEY",
        "firecrawl" => "FIRECRAWL_API_KEY",
        "browserbase" => "BROWSERBASE_API_KEY",
        "google" | "gemini" => "GEMINI_API_KEY",
        _ => return None,
    })
}

/// The vendors detected for a never-configured category, in order.
const DETECTED: &[(Category, &[&str])] = &[
    (
        Category::WebSearch,
        &["brave", "tavily", "exa", "firecrawl"],
    ),
    (Category::WebExtract, &["firecrawl", "tavily", "exa"]),
    (Category::ImageGeneration, &["fal", "openai", "xai"]),
    (Category::VideoGeneration, &["fal", "xai"]),
    (Category::Tts, &["elevenlabs", "openai"]),
    (Category::Stt, &["deepgram", "openai"]),
];

/// Where a category's calls go.
#[derive(Debug, Clone)]
pub enum ToolRouting {
    /// Through the subscription's tool gateway.
    Gateway {
        /// The gateway URL.
        url: String,
        /// The bearer token.
        token: SecretString,
        /// `subscription:<provider>/<label>` (or `api_key:...` for a
        /// self-hosted gateway's own token).
        billing_source: String,
    },
    /// To the vendor directly.
    Vendor {
        /// The vendor.
        vendor: String,
        /// Its key, when it takes one.
        key: Option<SecretString>,
    },
    /// Nothing configured or detected.
    Unset,
}

/// Why a category routes where it does, as `portal tools` shows it.
#[derive(Debug, Clone, Serialize)]
pub struct RouteView {
    /// The category.
    pub category: Category,
    /// The key holding the selection.
    pub key: &'static str,
    /// The stored selection; `None` when never configured.
    pub selection: Option<String>,
    /// `gateway`, `vendor`, `unset` or `error`.
    pub route: &'static str,
    /// The gateway URL or vendor, or the error.
    pub detail: String,
}

/// What routing reads.
pub struct Routing<'a> {
    /// The profile config.
    pub config: &'a Config,
    /// The profile's secrets.
    pub secrets: &'a dyn SecretSource,
    /// Sign-ins.
    pub extras: &'a dyn CredentialExtras,
}

impl Routing<'_> {
    /// Where `category`'s calls go.
    ///
    /// # Errors
    ///
    /// `provider.missing_credential` naming the selection key when the
    /// selected vendor's key (or the gateway's sign-in) is missing;
    /// `config.invalid` for a plain-HTTP public gateway URL; store errors.
    pub fn route(&self, category: Category) -> Result<ToolRouting, Error> {
        match category.selection(self.config) {
            Some((vendor, key)) if vendor == GATEWAY_PROVIDER => self.gateway(category, Some(key)),
            Some((vendor, key)) => self.vendor(&vendor, Some(key)),
            None => self.detect(category),
        }
    }

    /// Every category's routing, for `portal tools`.
    #[must_use]
    pub fn views(&self) -> Vec<RouteView> {
        Category::ALL
            .into_iter()
            .map(|category| {
                let selection = category.selection(self.config);
                let key = selection.as_ref().map_or(category.key(), |(_, k)| *k);
                let (route, detail) = match self.route(category) {
                    Ok(ToolRouting::Gateway { url, .. }) => ("gateway", url),
                    Ok(ToolRouting::Vendor { vendor, .. }) => ("vendor", vendor),
                    Ok(ToolRouting::Unset) => ("unset", String::new()),
                    Err(err) => ("error", err.message().to_owned()),
                };
                RouteView {
                    category,
                    key,
                    selection: selection.map(|(v, _)| v),
                    route,
                    detail,
                }
            })
            .collect()
    }

    fn gateway(&self, category: Category, key: Option<&str>) -> Result<ToolRouting, Error> {
        let gateway = &self.config.tool_gateway;
        let url = gateway.url(category.label());
        check_url(&url, false).map_err(|_| {
            Error::new(
                &codes::CONFIG_INVALID,
                format!("tool gateway URL '{url}' must be https:// (plain http only on a loopback or private host)"),
            )
        })?;
        if let Some(secret) = &gateway.token_secret {
            let token = self.secrets.secret(&secret.0)?.ok_or_else(|| {
                missing(
                    &format!("the tool gateway token {}", secret.0),
                    "tool_gateway.token_secret",
                )
            })?;
            return Ok(ToolRouting::Gateway {
                url,
                token,
                billing_source: format!("api_key:{GATEWAY_PROVIDER}/tool_gateway"),
            });
        }
        let signed_in = self.extras.subscription(GATEWAY_PROVIDER)?.ok_or_else(|| {
            missing("a Nous Portal sign-in", key.unwrap_or(category.key()))
                .with_hint("prometheus auth add nous")
        })?;
        Ok(ToolRouting::Gateway {
            url,
            token: signed_in.token,
            billing_source: format!("subscription:{GATEWAY_PROVIDER}/{}", signed_in.label),
        })
    }

    fn vendor(&self, vendor: &str, key: Option<&str>) -> Result<ToolRouting, Error> {
        let Some(secret) = vendor_secret(vendor) else {
            return Ok(ToolRouting::Vendor {
                vendor: vendor.to_owned(),
                key: None,
            });
        };
        match self.secrets.secret(secret)? {
            Some(value) => Ok(ToolRouting::Vendor {
                vendor: vendor.to_owned(),
                key: Some(value),
            }),
            None => Err(
                missing(&format!("{vendor}'s key {secret}"), key.unwrap_or("?"))
                    .with_hint(format!("prometheus secrets set {secret}")),
            ),
        }
    }

    /// A never-configured category: the first detected vendor with a key,
    /// else the gateway when the subscription is signed in.
    fn detect(&self, category: Category) -> Result<ToolRouting, Error> {
        let vendors = DETECTED
            .iter()
            .find(|(c, _)| *c == category)
            .map_or(&[][..], |(_, v)| *v);
        for vendor in vendors {
            let secret = vendor_secret(vendor).unwrap_or_default();
            if let Some(key) = self.secrets.secret(secret)? {
                return Ok(ToolRouting::Vendor {
                    vendor: (*vendor).to_owned(),
                    key: Some(key),
                });
            }
        }
        if self.extras.subscription(GATEWAY_PROVIDER)?.is_some() {
            return self.gateway(category, None);
        }
        Ok(ToolRouting::Unset)
    }
}

/// `provider.missing_credential` for `what`, naming the selection `key`.
fn missing(what: &str, key: &str) -> Error {
    Error::new(
        &codes::PROVIDER_MISSING_CREDENTIAL,
        format!("{key} selects a provider that needs {what}, which is not set"),
    )
}

/// The error of a gateway call the gateway refused for the plan (HTTP 402
/// or 403 after the subscription expired): `auth.plan_not_eligible`
/// naming the portal; `None` for other statuses.
#[must_use]
pub fn refused(status: u16, billing_source: &str) -> Option<Error> {
    if !matches!(status, 402 | 403) {
        return None;
    }
    let label = billing_source
        .rsplit_once('/')
        .map_or("default", |(_, label)| label);
    Some(
        crate::classify::plan_not_eligible(
            GATEWAY_PROVIDER,
            label,
            "the tool gateway refused the call: the subscription is not active",
        )
        .with_hint(format!("renew the subscription at {PORTAL_URL}")),
    )
}

/// Posts `body` to `path` of the gateway `routing` names, with its token:
/// the seam the gateway-routed tools call.
///
/// # Errors
///
/// `auth.plan_not_eligible` naming the portal when the gateway refuses the
/// plan (HTTP 402/403), `provider.call_failed` for other failures,
/// `config.invalid` when `routing` is not a gateway.
pub async fn post(
    client: &reqwest::Client,
    routing: &ToolRouting,
    path: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, Error> {
    let ToolRouting::Gateway {
        url,
        token,
        billing_source,
    } = routing
    else {
        return Err(Error::new(
            &codes::CONFIG_INVALID,
            "the category is not routed through the tool gateway",
        ));
    };
    let failed = |why: String| {
        Error::new(
            &codes::PROVIDER_CALL_FAILED,
            format!("tool gateway {url}: {why}"),
        )
    };
    let response = client
        .post(format!(
            "{}/{}",
            url.trim_end_matches('/'),
            path.trim_start_matches('/')
        ))
        .bearer_auth(token.expose())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| failed(e.to_string()))?;
    let status = response.status().as_u16();
    if let Some(err) = refused(status, billing_source) {
        return Err(err);
    }
    if !response.status().is_success() {
        return Err(failed(format!("HTTP {status}")));
    }
    let bytes = response.bytes().await.map_err(|e| failed(e.to_string()))?;
    serde_json::from_slice(&bytes).map_err(|e| failed(e.to_string()))
}
