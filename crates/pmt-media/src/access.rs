//! Who serves a media category and with which credential (spec:
//! web-and-media-tools, Media tool credentials and billing are explicit;
//! model-providers, Subscription tool gateway routing).
//!
//! A stored selection is always honoured: `nous` goes through the tool
//! gateway, any other id is that provider, whose missing key fails the
//! call naming it. A category never configured is detected by the gateway
//! routing (a vendor key, else a subscription sign-in), else falls back to
//! the category's default. Every credential carries its `billing_source`.

use std::sync::Arc;

use pmt_config::Config;
use pmt_core::secret::SecretString;
use pmt_providers::auth::gateway::{Category, GATEWAY_PROVIDER, Routing, ToolRouting};
use pmt_providers::resolve::{CredentialExtras, SecretSource};

/// The profile's credentials as the media tools read them.
#[derive(Clone)]
pub struct Credentials {
    /// The profile's configuration.
    pub config: Arc<Config>,
    /// Its secrets store.
    pub secrets: Arc<dyn SecretSource>,
    /// Its sign-ins.
    pub extras: Arc<dyn CredentialExtras>,
}

/// A credential for one call.
#[derive(Debug, Clone)]
pub struct Access {
    /// The bearer token or key (`None`: keyless or local).
    pub token: Option<SecretString>,
    /// How the call bills.
    pub billing_source: String,
    /// The gateway URL, for a gateway-routed call.
    pub gateway_url: Option<String>,
}

impl Access {
    /// A keyless call to `vendor`.
    #[must_use]
    pub fn keyless(vendor: &str) -> Self {
        Self {
            token: None,
            billing_source: format!("keyless:{vendor}"),
            gateway_url: None,
        }
    }

    /// An on-device engine.
    #[must_use]
    pub fn local() -> Self {
        Self {
            token: None,
            billing_source: "local".to_owned(),
            gateway_url: None,
        }
    }

    /// The token, or why it is missing.
    ///
    /// # Errors
    ///
    /// No token.
    pub fn bearer(&self) -> Result<&SecretString, String> {
        self.token
            .as_ref()
            .ok_or_else(|| "no credential for this provider".to_owned())
    }
}

/// The provider chosen for a category.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// The provider id (`nous` for the gateway).
    pub provider: String,
    /// The selection was explicit (`<category>.provider` set).
    pub explicit: bool,
}

impl Credentials {
    /// The value of secret `name`, if set.
    #[must_use]
    pub fn secret(&self, name: &str) -> Option<SecretString> {
        self.secrets.secret(name).ok().flatten()
    }

    /// A usable sign-in to `provider`: its token and label.
    #[must_use]
    pub fn signed_in(&self, provider: &str) -> Option<(SecretString, String)> {
        self.extras
            .subscription(provider)
            .ok()
            .flatten()
            .filter(|s| !s.ineligible)
            .map(|s| (s.token, s.label))
    }

    fn routing(&self) -> Routing<'_> {
        Routing {
            config: &self.config,
            secrets: self.secrets.as_ref(),
            extras: self.extras.as_ref(),
        }
    }

    /// The provider of `category`: the stored `selected` one, else the
    /// detected one, else `default`.
    #[must_use]
    pub fn choose(&self, category: Category, selected: Option<&str>, default: &str) -> Choice {
        if let Some(id) = selected {
            return Choice {
                provider: id.to_owned(),
                explicit: true,
            };
        }
        let provider = match self.routing().route(category) {
            Ok(ToolRouting::Gateway { .. }) => GATEWAY_PROVIDER.to_owned(),
            Ok(ToolRouting::Vendor { vendor, .. }) => vendor,
            Ok(ToolRouting::Unset) | Err(_) => default.to_owned(),
        };
        Choice {
            provider,
            explicit: false,
        }
    }

    /// The tool gateway credential of `category`.
    ///
    /// # Errors
    ///
    /// No sign-in or gateway token; a plain-HTTP public gateway URL.
    pub fn gateway(&self, category: Category) -> Result<Access, String> {
        match self.routing().route(category) {
            Ok(ToolRouting::Gateway {
                url,
                token,
                billing_source,
            }) => Ok(Access {
                token: Some(token),
                billing_source,
                gateway_url: Some(url),
            }),
            Ok(_) => Err("the category is not routed through the tool gateway".to_owned()),
            Err(err) => Err(err.message().to_owned()),
        }
    }

    /// The API key of `vendor` held in `secret`.
    ///
    /// # Errors
    ///
    /// `<secret> is not set`.
    pub fn key(&self, vendor: &str, secret: &str) -> Result<Access, String> {
        let token = self
            .secret(secret)
            .ok_or_else(|| format!("{secret} is not set"))?;
        Ok(Access {
            token: Some(token),
            billing_source: format!("api_key:{vendor}"),
            gateway_url: None,
        })
    }

    /// The sign-in of `provider`, billed to its subscription.
    ///
    /// # Errors
    ///
    /// No usable sign-in.
    pub fn subscription(&self, provider: &str) -> Result<Access, String> {
        let (token, label) = self
            .signed_in(provider)
            .ok_or_else(|| format!("no {provider} sign-in (prometheus auth add {provider})"))?;
        Ok(Access {
            token: Some(token),
            billing_source: format!("subscription:{provider}/{label}"),
            gateway_url: None,
        })
    }

    /// The xAI credential of a tool: the API key first, then the
    /// `xai-oauth` sign-in.
    ///
    /// # Errors
    ///
    /// Neither exists.
    pub fn xai_key_first(&self) -> Result<Access, String> {
        self.key("xai", "XAI_API_KEY")
            .or_else(|_| self.subscription("xai-oauth"))
            .map_err(|_| "no xAI API key (XAI_API_KEY) or xai-oauth sign-in".to_owned())
    }
}
