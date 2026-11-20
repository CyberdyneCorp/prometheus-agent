//! The session surface a selection starts from (spec: tool-system,
//! Per-surface default toolsets; Platform bundles).
//!
//! A session's starting toolsets are `toolsets.surfaces.<surface>` when
//! set, else `tools.enabled_toolsets` with `core` standing for the
//! surface's built-in default: `core` for cli, tui, desktop and cron,
//! `api_server` for the API server and inbound A2A, `acp`/`acp_unattended` for ACP and
//! `platform-<platform>` for a messaging platform. Webhook-triggered runs
//! never fall back to the profile-wide list: their payload comes from
//! outside, so they start from `webhook-safe` unless
//! `toolsets.surfaces.gateway.webhook` (or the route) names toolsets.

use std::fmt;

use pmt_config::schema::{ToolsConfig, ToolsetsConfig};

use crate::agent_level_desktop::{CATALOG, DESKTOP_UI, ONBOARDING, PROJECT};
use crate::registry::ToolRegistry;
use crate::toolset;

/// The platform whose sessions are webhook-triggered runs.
pub const WEBHOOK: &str = "webhook";

/// The surface a session was started from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionSurface {
    /// Line-mode chat and one-shot runs.
    Cli,
    /// The terminal UI.
    Tui,
    /// The desktop app.
    Desktop,
    /// The desktop's first-run onboarding chat: a desktop session that
    /// also gets the `onboarding` toolset (configured as `desktop`).
    Onboarding,
    /// The web dashboard's chat.
    Dashboard,
    /// A scheduled job.
    Cron,
    /// The OpenAI-compatible API server.
    Api,
    /// An inbound A2A conversation (its toolsets come from
    /// `a2a.server.toolsets`).
    A2a,
    /// An ACP editor session.
    Acp,
    /// An unattended ACP session.
    AcpUnattended,
    /// A messaging platform's session (`webhook` for webhook-triggered
    /// runs).
    Gateway(String),
}

impl SessionSurface {
    /// The surface of key `key` (`cli`, `gateway.telegram`...).
    #[must_use]
    pub fn parse(key: &str) -> Option<Self> {
        if let Some(platform) = key.strip_prefix("gateway.") {
            return valid_platform(platform).then(|| Self::Gateway(platform.to_lowercase()));
        }
        Some(match key {
            "cli" => Self::Cli,
            "tui" => Self::Tui,
            "desktop" => Self::Desktop,
            "dashboard" => Self::Dashboard,
            "cron" => Self::Cron,
            "api" => Self::Api,
            "a2a" => Self::A2a,
            "acp" => Self::Acp,
            "acp_unattended" => Self::AcpUnattended,
            _ => return None,
        })
    }

    /// The `toolsets.surfaces` key.
    #[must_use]
    pub fn key(&self) -> String {
        match self {
            Self::Cli => "cli".to_owned(),
            Self::Tui => "tui".to_owned(),
            Self::Desktop | Self::Onboarding => "desktop".to_owned(),
            Self::Dashboard => "dashboard".to_owned(),
            Self::Cron => "cron".to_owned(),
            Self::Api => "api".to_owned(),
            Self::A2a => "a2a".to_owned(),
            Self::Acp => "acp".to_owned(),
            Self::AcpUnattended => "acp_unattended".to_owned(),
            Self::Gateway(platform) => format!("gateway.{platform}"),
        }
    }

    /// Whether it is the webhook platform's surface.
    #[must_use]
    pub fn is_webhook(&self) -> bool {
        matches!(self, Self::Gateway(p) if p == WEBHOOK)
    }

    /// The built-in default toolset. A messaging platform without a
    /// bundle (the relay, an internal platform) starts from `core`.
    #[must_use]
    pub fn default_toolset(&self, registry: &ToolRegistry) -> String {
        match self {
            Self::Cli
            | Self::Tui
            | Self::Desktop
            | Self::Onboarding
            | Self::Dashboard
            | Self::Cron => "core".to_owned(),
            Self::Api | Self::A2a => "api_server".to_owned(),
            Self::Acp => "acp".to_owned(),
            Self::AcpUnattended => "acp_unattended".to_owned(),
            Self::Gateway(_) if self.is_webhook() => "webhook-safe".to_owned(),
            Self::Gateway(platform) => {
                let bundle = toolset::bundle_name(platform);
                if registry.toolset(&bundle).is_some() {
                    bundle
                } else {
                    "core".to_owned()
                }
            }
        }
    }

    /// The toolsets a session of this surface starts from, before
    /// `tools.disabled_toolsets` is subtracted.
    #[must_use]
    pub fn starting_toolsets(
        &self,
        registry: &ToolRegistry,
        tools: &ToolsConfig,
        toolsets: &ToolsetsConfig,
    ) -> Vec<String> {
        if let Some(list) = toolsets.surfaces.get(&self.key()) {
            return list.clone();
        }
        let default = self.default_toolset(registry);
        if self.is_webhook() {
            return vec![default];
        }
        let mut out: Vec<String> = Vec::new();
        for name in &tools.enabled_toolsets {
            let name = if name == "core" { &default } else { name };
            if !out.contains(name) {
                out.push(name.clone());
            }
        }
        out
    }

    /// The surface-gated toolsets this surface folds in (spec:
    /// tool-system, Surface toolsets come from the session; desktop-app,
    /// Desktop UI toolset contract; First-run onboarding chat; projects,
    /// Project tool for GUI sessions): `desktop_ui`, `project` and
    /// `catalog` for desktop sessions (plus `onboarding` for the
    /// onboarding chat), `project` and `catalog` for the TUI, `project`
    /// for the dashboard. Only the session's own surface decides: no
    /// other surface, environment variable or process ever gets them.
    #[must_use]
    pub fn folded_toolsets(&self) -> &'static [&'static str] {
        match self {
            Self::Desktop => &[DESKTOP_UI, PROJECT, CATALOG],
            Self::Onboarding => &[DESKTOP_UI, ONBOARDING, PROJECT, CATALOG],
            Self::Tui => &[PROJECT, CATALOG],
            Self::Dashboard => &[PROJECT],
            _ => &[],
        }
    }
}

impl fmt::Display for SessionSurface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.key())
    }
}

fn valid_platform(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}
