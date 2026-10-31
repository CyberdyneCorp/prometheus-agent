//! The tool configurator (spec: cli, Tool configurator): `prometheus tools
//! configure [--surface S]` (interactive), `tools enable|disable <toolset>
//! [--surface S]` and `tools provider <category> <provider>`. It writes
//! only through the config writer, refuses unknown toolsets, surfaces,
//! categories and providers with exit 78 listing the valid names, and its
//! changes apply from the next session.

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_tools::ToolRegistry;

use crate::context::Context;
use crate::output::print;
use crate::setup_cmd::{Wizard, write_changed};

/// Surfaces a toolset selection can be saved for (`gateway.<platform>`
/// too).
pub const SURFACES: &[&str] = &["cli", "tui", "cron", "acp"];

/// A provider-backed tool category.
#[derive(Debug, Clone, Copy)]
pub struct Category {
    /// The name `tools provider` takes.
    pub name: &'static str,
    /// The config key it writes (`None`: no provider in this build).
    pub key: Option<&'static str>,
    /// The registered providers.
    pub providers: &'static [&'static str],
}

/// Every category; the ones without a registered provider in this build
/// refuse every name.
pub const CATEGORIES: &[Category] = &[
    Category {
        name: "web_search",
        key: Some("web.search_backend"),
        providers: &["auto", "firecrawl", "searxng"],
    },
    Category {
        name: "web_extract",
        key: Some("web.extract_backend"),
        providers: &["auto", "firecrawl"],
    },
    Category {
        name: "browser",
        key: Some("browser.provider"),
        providers: &["local"],
    },
    Category {
        name: "image_generation",
        key: None,
        providers: &[],
    },
    Category {
        name: "tts",
        key: None,
        providers: &[],
    },
    Category {
        name: "stt",
        key: None,
        providers: &[],
    },
    Category {
        name: "video",
        key: None,
        providers: &[],
    },
    Category {
        name: "x_search",
        key: None,
        providers: &[],
    },
];

fn names(list: &[&str]) -> String {
    if list.is_empty() {
        "none in this build".to_owned()
    } else {
        list.join(", ")
    }
}

/// The category `name`.
///
/// # Errors
///
/// `tools.unknown_provider` (78) listing the categories.
pub fn category(name: &str) -> Result<&'static Category, Error> {
    CATEGORIES.iter().find(|c| c.name == name).ok_or_else(|| {
        let all: Vec<&str> = CATEGORIES.iter().map(|c| c.name).collect();
        Error::new(
            &codes::TOOLS_UNKNOWN_PROVIDER,
            format!(
                "unknown tool category '{name}'; categories: {}",
                all.join(", ")
            ),
        )
    })
}

/// Checks `provider` against `category`'s registered providers; returns
/// the config key to write.
///
/// # Errors
///
/// `tools.unknown_provider` (78) listing the registered providers.
pub fn check_provider(category: &Category, provider: &str) -> Result<&'static str, Error> {
    match category.key {
        Some(key) if category.providers.contains(&provider) => Ok(key),
        _ => Err(Error::new(
            &codes::TOOLS_UNKNOWN_PROVIDER,
            format!(
                "unknown {} provider '{provider}'; registered: {}",
                category.name,
                names(category.providers)
            ),
        )),
    }
}

/// Checks a surface name.
///
/// # Errors
///
/// `tools.unknown_provider` (78) listing the surfaces.
pub fn check_surface(surface: &str) -> Result<(), Error> {
    let gateway = surface.strip_prefix("gateway.").is_some_and(|p| {
        !p.is_empty()
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    });
    if SURFACES.contains(&surface) || gateway {
        return Ok(());
    }
    Err(Error::new(
        &codes::TOOLS_UNKNOWN_PROVIDER,
        format!(
            "unknown surface '{surface}'; surfaces: {}, gateway.<platform>",
            SURFACES.join(", ")
        ),
    ))
}

/// Saves a toolset change for the next sessions: `tools.disabled_toolsets`
/// gains or loses the toolset (and `tools.enabled_toolsets` gains it when
/// enabling).
///
/// # Errors
///
/// Config write errors.
pub fn persist_toolset(scope: &ProfileScope, toolset: &str, enable: bool) -> Result<(), Error> {
    let file = scope.profile().config_file();
    let (text, _) = file.read()?;
    let saved: pmt_config::Config = toml::from_str(&text).unwrap_or_default();
    let tools = &saved.tools;
    let mut disabled = tools.disabled_toolsets.clone();
    let mut enabled = tools.enabled_toolsets.clone();
    disabled.retain(|t| t != toolset);
    if enable {
        if !enabled.iter().any(|t| t == toolset) {
            enabled.push(toolset.to_owned());
        }
    } else {
        disabled.push(toolset.to_owned());
    }
    let list = |names: &[String]| {
        let quoted: Vec<String> = names.iter().map(|n| format!("{n:?}")).collect();
        format!("[{}]", quoted.join(", "))
    };
    file.set_all(&[
        ("tools.enabled_toolsets", &list(&enabled)),
        ("tools.disabled_toolsets", &list(&disabled)),
    ])
}

/// The toolsets `surface` gets: its saved list, else the profile's.
fn surface_toolsets(scope: &ProfileScope, surface: Option<&str>) -> Vec<String> {
    let config = scope.config();
    surface
        .and_then(|s| config.toolsets.surfaces.get(s))
        .unwrap_or(&config.tools.enabled_toolsets)
        .clone()
}

fn toml_list(names: &[String]) -> String {
    let quoted: Vec<String> = names.iter().map(|n| format!("{n:?}")).collect();
    format!("[{}]", quoted.join(", "))
}

/// Enables or disables `toolset` for `surface` (all surfaces when
/// `None`: `tools.enabled_toolsets` / `tools.disabled_toolsets`).
///
/// # Errors
///
/// `toolset` unknown or a posture toolset, `surface` unknown, config
/// write errors.
pub fn set_toolset(
    scope: &ProfileScope,
    registry: &ToolRegistry,
    toolset: &str,
    enable: bool,
    surface: Option<&str>,
) -> Result<(), Error> {
    let Some(found) = registry.toolsets().find(|t| t.name == toolset) else {
        return Err(registry.unknown_toolset(toolset));
    };
    if found.posture {
        return Err(Error::new(
            &codes::TOOLS_UNKNOWN_PROVIDER,
            format!("'{toolset}' is a posture toolset, chosen per session; it is never saved"),
        ));
    }
    let Some(surface) = surface else {
        return persist_toolset(scope, toolset, enable);
    };
    check_surface(surface)?;
    let mut list = surface_toolsets(scope, Some(surface));
    list.retain(|t| t != toolset);
    if enable {
        list.push(toolset.to_owned());
    }
    write_changed(
        scope,
        &[(&format!("toolsets.surfaces.{surface}"), toml_list(&list))],
    )
    .map(drop)
}

/// `tools enable|disable <toolset> [--surface S]`.
///
/// # Errors
///
/// As [`set_toolset`].
pub fn enable(
    ctx: &Context<'_>,
    toolset: &str,
    on: bool,
    surface: Option<&str>,
) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let registry = super::registry(scope.name().as_str())?;
    set_toolset(&scope, &registry, toolset, on, surface)?;
    let verb = if on { "enabled" } else { "disabled" };
    let place = surface.map_or_else(String::new, |s| format!(" for {s}"));
    print(&format!(
        "toolset {toolset} {verb}{place}; applies from the next session"
    ))
}

/// `tools provider <category> <provider>`.
///
/// # Errors
///
/// `tools.unknown_provider` (78); config write errors.
pub fn provider(ctx: &Context<'_>, category_name: &str, provider: &str) -> Result<u8, Error> {
    let category = category(category_name)?;
    let key = check_provider(category, provider)?;
    let scope = ctx.scope()?;
    write_changed(&scope, &[(key, provider.to_owned())])?;
    print(&format!(
        "{key} = \"{provider}\"; applies from the next session"
    ))
}

/// `tools configure [--surface S]` (and `setup tools`).
///
/// # Errors
///
/// As [`set_toolset`] and [`provider`].
pub(crate) fn interactive(
    ctx: &Context<'_>,
    surface: Option<&str>,
    wizard: &mut Wizard<'_>,
) -> Result<(), Error> {
    if let Some(surface) = surface {
        check_surface(surface)?;
    }
    toggle_toolsets(ctx, surface, wizard)?;
    choose_providers(ctx, wizard)?;
    wizard.say("tool settings apply from the next session")
}

fn toggle_toolsets(
    ctx: &Context<'_>,
    surface: Option<&str>,
    wizard: &mut Wizard<'_>,
) -> Result<(), Error> {
    let scope = ctx.scope()?;
    let registry = super::registry(scope.name().as_str())?;
    let enabled = surface_toolsets(&scope, surface);
    let toolsets: Vec<String> = registry
        .toolsets()
        .filter(|t| !t.posture)
        .map(|t| t.name.clone())
        .collect();
    wizard.say(&format!(
        "Toolsets for {}:",
        surface.unwrap_or("every surface")
    ))?;
    for (n, name) in toolsets.iter().enumerate() {
        let mark = if enabled.contains(name) { "on " } else { "off" };
        wizard.say(&format!("{:>3}) [{mark}] {name}", n + 1))?;
    }
    while let Some(answer) = wizard.ask("toggle a toolset (number or name; Enter to go on):")? {
        let index = answer.parse::<usize>().ok().and_then(|n| n.checked_sub(1));
        let Some(name) = index
            .and_then(|i| toolsets.get(i))
            .or_else(|| toolsets.iter().find(|t| **t == answer))
        else {
            wizard.say(&format!("no toolset '{answer}'"))?;
            continue;
        };
        let now = surface_toolsets(&ctx.scope()?, surface).contains(name);
        set_toolset(&ctx.scope()?, &registry, name, !now, surface)?;
        wizard.say(&format!("{name}: {}", if now { "off" } else { "on" }))?;
    }
    Ok(())
}

fn choose_providers(ctx: &Context<'_>, wizard: &mut Wizard<'_>) -> Result<(), Error> {
    for category in CATEGORIES {
        let Some(key) = category.key else {
            continue;
        };
        let current = ctx
            .scope()?
            .layers()
            .value(key)
            .map_or_else(|| "unset".to_owned(), crate::setup_cmd::plain);
        let question = format!(
            "{} provider ({}; current {current}; Enter keeps it):",
            category.name,
            names(category.providers)
        );
        let Some(answer) = wizard.ask(&question)? else {
            continue;
        };
        match check_provider(category, &answer) {
            Ok(key) => {
                write_changed(&ctx.scope()?, &[(key, answer)])?;
            }
            Err(err) => wizard.say(err.message())?,
        }
    }
    Ok(())
}

/// `prometheus tools configure`.
///
/// # Errors
///
/// As [`interactive`].
pub fn configure(ctx: &Context<'_>, surface: Option<&str>) -> Result<u8, Error> {
    let mut wizard = Wizard::stdio();
    interactive(ctx, surface, &mut wizard)?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: Unknown provider refused (the message lists the
    /// registered web search providers).
    #[test]
    fn unknown_providers_categories_and_surfaces_are_refused() {
        let err = check_provider(category("web_search").unwrap(), "nosuch").unwrap_err();
        assert_eq!(err.exit_code(), 78);
        assert!(
            err.message().contains("auto, firecrawl, searxng"),
            "{}",
            err.message()
        );
        assert!(check_provider(category("tts").unwrap(), "x").is_err());
        assert_eq!(category("weather").unwrap_err().exit_code(), 78);
        assert!(check_surface("gateway.telegram").is_ok());
        assert!(check_surface("desk").is_err());
        assert_eq!(
            check_provider(category("web_extract").unwrap(), "firecrawl").unwrap(),
            "web.extract_backend"
        );
    }
}
