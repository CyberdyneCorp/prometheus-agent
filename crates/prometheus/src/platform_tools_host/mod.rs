//! The adapters' own tools (spec: platform-adapters, Platform-contributed
//! toolsets): every adapter of this build that contributes tools is
//! declared once per process, before the first tool registry is built.
//! Whether a profile sees them is `pmt_tools::platform_tools::register`'s
//! call (only for the platforms it enables). A catalog adapter plugin
//! cannot contribute tools over its WIT world, so its platform's toolset
//! lives here ([`feishu`], [`homeassistant`], [`yuanbao`]).

pub mod feishu;
pub mod homeassistant;
pub mod yuanbao;

/// Declares every contributing adapter's tools (once).
pub fn contribute_all() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        pmt_tools::platform_tools::contribute(std::sync::Arc::new(feishu::Tools));
        pmt_tools::platform_tools::contribute(std::sync::Arc::new(homeassistant::Tools));
        pmt_tools::platform_tools::contribute(std::sync::Arc::new(yuanbao::Tools));
        #[cfg(feature = "gateway")]
        pmt_tools::platform_tools::contribute(std::sync::Arc::new(
            pmt_adapters_discord::DiscordTools::default(),
        ));
    });
}
