//! What `prometheus tools explain` and `prometheus doctor` show for every
//! web and media category (spec: web-and-media-tools, Media tool
//! credentials and billing are explicit): the resolved provider, its
//! billing source and why the category is unavailable when it is.

use std::path::Path;
use std::sync::Arc;

use pmt_config::schema::WebConfig;
use pmt_net::{DestinationPolicy, Fetcher, OutboundPolicy};
use pmt_tools::web::Capability;
use serde::Serialize;

use crate::access::Credentials;
use crate::speech::local::Launcher;
use crate::{MediaSession, MediaSetup};

/// One category as shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CategoryView {
    /// The category (`web_search`, `image_generation`, `tts`...).
    pub category: &'static str,
    /// The tools it serves.
    pub tools: &'static [&'static str],
    /// The resolved provider.
    pub provider: Option<String>,
    /// How its calls bill.
    pub billing_source: Option<String>,
    /// `available`, or `unavailable:<reason>`.
    pub availability: String,
}

impl CategoryView {
    fn of(
        category: &'static str,
        tools: &'static [&'static str],
        resolved: Result<(String, String), String>,
    ) -> Self {
        match resolved {
            Ok((provider, billing)) => Self {
                category,
                tools,
                provider: Some(provider),
                billing_source: Some(billing),
                availability: "available".to_owned(),
            },
            Err(why) => Self {
                category,
                tools,
                provider: None,
                billing_source: None,
                availability: format!("unavailable:{why}"),
            },
        }
    }
}

/// The billing source of web backend `id` as configured.
fn web_billing(id: &str, web: &WebConfig, credentials: &Credentials) -> String {
    match id {
        "searxng" => "local".to_owned(),
        "ddgs" => "keyless:ddgs".to_owned(),
        "keyless" => "keyless:<ring vendor>".to_owned(),
        "openai-native" => "the main route (openai-codex)".to_owned(),
        "firecrawl"
            if web.firecrawl.base_url.is_some()
                && credentials.secret("FIRECRAWL_API_KEY").is_none() =>
        {
            "local".to_owned()
        }
        "nous" => credentials
            .gateway(pmt_providers::auth::gateway::Category::WebSearch)
            .map_or_else(|_| "subscription:nous".to_owned(), |a| a.billing_source),
        "xai" => credentials
            .xai_key_first()
            .map_or_else(|_| "api_key:xai".to_owned(), |a| a.billing_source),
        other => format!("api_key:{other}"),
    }
}

/// A setup for resolving providers only (no call is made).
#[must_use]
pub fn inspection_setup(
    credentials: Credentials,
    profile_dir: &Path,
    local: Option<Arc<dyn Launcher>>,
) -> Option<MediaSetup> {
    let fetcher = Fetcher::new(
        &OutboundPolicy::default(),
        DestinationPolicy::new(false, &[]).ok()?,
    )
    .ok()?;
    Some(MediaSetup {
        credentials,
        client: pmt_net::client(&OutboundPolicy::default()).ok()?,
        fetcher: Arc::new(fetcher),
        host: Arc::new(crate::DirHost(profile_dir.join("cache").join("media"))),
        profile_dir: profile_dir.to_path_buf(),
        workspace: profile_dir.to_path_buf(),
        surface: "cli".to_owned(),
        endpoints: std::collections::BTreeMap::new(),
        usage: None,
        video: None,
        runner: Arc::new(crate::speech::command::PlainShell),
        local,
        plugins: Arc::new(crate::plugin::NoPlugins),
    })
}

/// Every web and media category of the profile; `has_secret` answers
/// presence (`signin:<provider>` for sign-ins).
#[must_use]
pub fn categories(
    setup: MediaSetup,
    has_secret: &(dyn Fn(&str) -> bool + Send + Sync),
) -> Vec<CategoryView> {
    let credentials = setup.credentials.clone();
    let web = &credentials.config.web;
    let web_view = |category, tools, capability| {
        let resolved = pmt_tools::web::resolve(web, capability, has_secret)
            .map(|id| (id.to_owned(), web_billing(id, web, &credentials)));
        CategoryView::of(category, tools, resolved)
    };
    let mut views = vec![
        web_view("web_search", &["web_search"], Capability::Search),
        web_view("web_extract", &["web_extract"], Capability::Extract),
        CategoryView::of(
            "x_search",
            &["x_search"],
            credentials
                .xai_key_first()
                .map(|a| ("xai".to_owned(), a.billing_source)),
        ),
        CategoryView::of("image_generation", &["image_generate"], image(&setup)),
        CategoryView {
            category: "video_analysis",
            tools: &["video_analyze"],
            provider: Some("auxiliary.vision".to_owned()),
            billing_source: None,
            availability: "per session: available when the vision auxiliary model takes video (supports_video)".to_owned(),
        },
    ];
    let tts = crate::speech::tts::resolve(&setup).and_then(|voice| {
        if let Some(plugin) = &voice.plugin {
            return Ok(plugin_view(&crate::plugin::Selected {
                id: voice.provider,
                plugin: plugin.clone(),
            }));
        }
        let local = crate::speech::tts::LOCAL.contains(&voice.provider.as_str());
        if local
            && !setup
                .local
                .as_ref()
                .is_some_and(|l| l.available(&voice.provider))
        {
            return Err(format!(
                "{} is not installed (no local speech runtime)",
                voice.provider
            ));
        }
        Ok((voice.provider, voice.access.billing_source))
    });
    views.push(CategoryView::of("tts", &["text_to_speech"], tts));
    let session = MediaSession::new(setup);
    views.push(CategoryView::of(
        "stt",
        &[],
        session.stt_choice().map(|t| (t.provider, t.billing_source)),
    ));
    views
}

/// The image provider and its billing: a plugin's, else the plan's.
fn image(setup: &MediaSetup) -> Result<(String, String), String> {
    if let Some(selected) = crate::plugin::selected(setup, crate::plugin::Kind::ImageGen) {
        return Ok(plugin_view(&selected));
    }
    crate::image::plan(&setup.credentials, &|id, default| {
        setup.endpoint(id, default)
    })
    .map(|p| {
        (
            format!("{} ({})", p.provider, p.model.id),
            p.access.billing_source,
        )
    })
}

fn plugin_view(selected: &crate::plugin::Selected) -> (String, String) {
    (
        format!("{} (plugin {})", selected.id, selected.plugin),
        format!("reported by plugin '{}'", selected.plugin),
    )
}

/// The category view of `tool`, if it is a web or media tool.
#[must_use]
pub fn of_tool<'a>(views: &'a [CategoryView], tool: &str) -> Option<&'a CategoryView> {
    views.iter().find(|v| v.tools.contains(&tool))
}
