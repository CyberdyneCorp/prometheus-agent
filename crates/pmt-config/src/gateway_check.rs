//! Whole-config checks of the messaging gateway keys (spec:
//! messaging-gateway, Authorization order and fail-closed default; Gateway
//! slash commands, quick commands and hooks): an allowlist never holds a
//! wildcard (admitting everyone is the explicit `allow_all`), and a quick
//! command never shadows a command of the shared registry.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::OnceLock;

use pmt_core::error::{Error, codes};

use crate::schema::{ChatPolicy, Config};

/// Expands toolset names to the tools they select; `Err(name)` for an
/// unknown toolset.
pub type ToolsetExpander = fn(&[String]) -> Result<BTreeSet<String>, String>;

/// The tool registry's toolset expansion, set once by the binary that owns
/// the registry. Unset (libraries, tests): a policy's toolsets must be
/// listed literally among the profile's.
static TOOLSETS: OnceLock<ToolsetExpander> = OnceLock::new();

/// Registers the toolset expansion used to check that chat policies only
/// narrow the profile's toolsets. The first call wins.
pub fn register_toolsets(expand: ToolsetExpander) {
    let _ = TOOLSETS.set(expand);
}

/// Finds the first of `names` that is not a skill of the profile in
/// `profile_dir` under `config` (`None`: all resolve), the way a session
/// resolves them.
pub type SkillResolver = Box<dyn Fn(&Config, &Path, &[String]) -> Option<String> + Send + Sync>;

/// Asks a connected adapter of the profile in `profile_dir` for the id of
/// a role name on a platform (`None`: no running daemon, adapter or role).
pub type RoleResolver = Box<dyn Fn(&Path, &str, &str) -> Option<String> + Send + Sync>;

static ROLES: OnceLock<RoleResolver> = OnceLock::new();

/// Registers the role lookup that suggests the id of an `allow_roles`
/// entry naming a role. The first call wins; unset, no id is suggested.
pub fn register_roles(resolve: RoleResolver) {
    let _ = ROLES.set(resolve);
}

/// The id a connected adapter knows role `name` by, if any.
pub(crate) fn resolve_role(profile_dir: &Path, platform: &str, name: &str) -> Option<String> {
    ROLES
        .get()
        .and_then(|resolve| resolve(profile_dir, platform, name))
}

/// The skill catalog lookup, set once by the binary that owns the catalog.
/// Unset (libraries, tests): only disabled skills are refused.
static SKILLS: OnceLock<SkillResolver> = OnceLock::new();

/// Registers the skill lookup used to refuse a chat policy naming an
/// unknown skill. The first call wins.
pub fn register_skills(resolve: SkillResolver) {
    let _ = SKILLS.set(resolve);
}

/// The shared slash command registry's names and aliases, set once by the
/// binary that owns the registry. Unset (libraries, tests): no quick
/// command name is reserved.
static RESERVED: OnceLock<Vec<String>> = OnceLock::new();

/// Reserves the registry's command names: a quick command named like one
/// fails validation from then on. The first call wins.
pub fn reserve_command_names(names: impl IntoIterator<Item = impl Into<String>>) {
    let _ = RESERVED.set(names.into_iter().map(Into::into).collect());
}

/// Refuses a wildcard allowlist entry, a quick command that collides
/// with a registry command and a chat policy that widens or names unknown
/// toolsets or skills of the profile in `profile_dir`.
///
/// # Errors
///
/// `config.invalid` naming the key.
pub fn check(config: &Config, profile_dir: &Path) -> Result<(), Error> {
    for (platform, table) in &config.gateway.platforms {
        if table.allow_users.iter().any(|user| user.contains('*')) {
            return Err(Error::new(
                &codes::CONFIG_INVALID,
                format!(
                    "gateway.platforms.{platform}.allow_users: wildcards are not allowed; set gateway.platforms.{platform}.allow_all = true to admit everyone"
                ),
            ));
        }
    }
    for (platform, table) in &config.gateway.platforms {
        if table
            .home_channel
            .as_ref()
            .is_some_and(|h| h.chat_id.trim().is_empty())
        {
            return Err(invalid(format!(
                "gateway.platforms.{platform}.home_channel.chat_id: must name the chat"
            )));
        }
    }
    check_platform_timing(config)?;
    crate::gateway_check_platforms::check(config)?;
    check_policies(config, profile_dir)?;
    crate::gateway_check_display::check(config, profile_dir)?;
    crate::gateway_check_groups::check(config, profile_dir)?;
    check_health(config)?;
    let reserved = RESERVED.get().map(Vec::as_slice).unwrap_or_default();
    for name in config.quick_commands.keys() {
        if reserved.iter().any(|r| r == name) {
            return Err(Error::new(
                &codes::CONFIG_INVALID,
                format!(
                    "quick_commands.{name}: /{name} is a built-in command; give the quick command another name"
                ),
            ));
        }
    }
    Ok(())
}

/// Batching waits after a split part at least as long as after any part,
/// and a human delay range starts at its minimum.
fn check_platform_timing(config: &Config) -> Result<(), Error> {
    for (platform, table) in &config.gateway.platforms {
        if let Some(batching) = &table.batching
            && batching.split_delay_ms < batching.delay_ms
        {
            return Err(invalid(format!(
                "gateway.platforms.{platform}.batching.split_delay_ms: must be at least delay_ms ({} ms)",
                batching.delay_ms
            )));
        }
        if let Some([min, max]) = table.human_delay_ms
            && min > max
        {
            return Err(invalid(format!(
                "gateway.platforms.{platform}.human_delay_ms: [min, max] with min <= max (found [{min}, {max}])"
            )));
        }
    }
    Ok(())
}

fn invalid(message: String) -> Error {
    Error::new(&codes::CONFIG_INVALID, message)
}

fn check_policies(config: &Config, profile_dir: &Path) -> Result<(), Error> {
    let policies = &config.gateway.chat_policies;
    for (index, policy) in policies.iter().enumerate() {
        let key = format!("gateway.chat_policies[{index}]");
        if policy.platform.trim().is_empty() {
            return Err(invalid(format!("{key}.platform: must name a platform")));
        }
        if policy.specificity() == 0 {
            return Err(invalid(format!(
                "{key}: match on chat_id, thread_id, chat_type or user_id besides the platform"
            )));
        }
        if let Some(earlier) = policies[..index].iter().position(|p| p.ties_with(policy)) {
            return Err(invalid(format!(
                "{key}: matches the same chats as gateway.chat_policies[{earlier}]; neither would win"
            )));
        }
        check_policy_toolsets(config, policy, &key)?;
        check_policy_skills(
            config,
            profile_dir,
            policy.skills.as_deref(),
            &format!("{key}.skills"),
        )?;
        check_policy_skills(
            config,
            profile_dir,
            policy.preload_skills.as_deref(),
            &format!("{key}.preload_skills"),
        )?;
    }
    Ok(())
}

/// The profile's toolsets as sessions select them: the per-surface list,
/// else `tools.enabled_toolsets`, minus the disabled ones.
fn profile_toolsets(config: &Config) -> Vec<String> {
    let enabled = config
        .toolsets
        .surfaces
        .get("cli")
        .unwrap_or(&config.tools.enabled_toolsets);
    enabled
        .iter()
        .filter(|t| !config.tools.disabled_toolsets.contains(t))
        .cloned()
        .collect()
}

fn check_policy_toolsets(config: &Config, policy: &ChatPolicy, key: &str) -> Result<(), Error> {
    let Some(toolsets) = &policy.toolsets else {
        return Ok(());
    };
    let profile = profile_toolsets(config);
    let outside = |name: &str| {
        invalid(format!(
            "{key}.toolsets: '{name}' is outside the profile's toolsets ({}); a policy can only narrow them",
            profile.join(", ")
        ))
    };
    let Some(expand) = TOOLSETS.get() else {
        return match toolsets.iter().find(|t| !profile.contains(t)) {
            Some(name) => Err(outside(name)),
            None => Ok(()),
        };
    };
    let allowed = expand(&profile)
        .map_err(|name| invalid(format!("tools.enabled_toolsets: unknown toolset '{name}'")))?;
    for name in toolsets {
        let tools = expand(std::slice::from_ref(name))
            .map_err(|_| invalid(format!("{key}.toolsets: unknown toolset '{name}'")))?;
        if !tools.is_subset(&allowed) {
            return Err(outside(name));
        }
    }
    Ok(())
}

/// A policy's skill list (`key`) names only enabled skills of the
/// profile.
fn check_policy_skills(
    config: &Config,
    profile_dir: &Path,
    skills: Option<&[String]>,
    key: &str,
) -> Result<(), Error> {
    let Some(skills) = skills.filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    if let Some(name) = skills.iter().find(|s| config.skills.disabled.contains(s)) {
        return Err(invalid(format!(
            "{key}: '{name}' is disabled in this profile (skills.disabled); a policy can only narrow the enabled skills"
        )));
    }
    let unknown = SKILLS
        .get()
        .and_then(|resolve| resolve(config, profile_dir, skills));
    match unknown {
        Some(name) => Err(invalid(format!(
            "{key}: unknown skill '{name}'; `prometheus skills list` shows the profile's skills"
        ))),
        None => Ok(()),
    }
}

fn check_health(config: &Config) -> Result<(), Error> {
    let health = &config.observability.gateway_health;
    if !health.enabled {
        return Ok(());
    }
    let Some(endpoint) = health
        .endpoint
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
    else {
        return Err(invalid(
            "observability.gateway_health.endpoint: required when observability.gateway_health.enabled = true".to_owned(),
        ));
    };
    if safe_endpoint(endpoint) {
        Ok(())
    } else {
        Err(invalid(format!(
            "observability.gateway_health.endpoint: '{endpoint}' must be https://, or http:// to a loopback collector"
        )))
    }
}

/// `https://`, or plain `http://` only to a loopback host.
fn safe_endpoint(endpoint: &str) -> bool {
    if endpoint.starts_with("https://") {
        return true;
    }
    let Some(rest) = endpoint.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.rsplit_once(':').map_or(authority, |(h, _)| h),
    };
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::PlatformConfig;

    #[test]
    fn a_wildcard_allowlist_points_to_allow_all() {
        let mut config = Config::default();
        config.gateway.platforms.insert(
            "telegram".into(),
            PlatformConfig {
                allow_users: vec!["*".into()],
                ..PlatformConfig::default()
            },
        );
        let err = check(&config, Path::new("")).unwrap_err();
        assert_eq!(err.code(), &codes::CONFIG_INVALID);
        assert!(
            err.message()
                .contains("gateway.platforms.telegram.allow_users")
        );
        assert!(err.message().contains("allow_all"), "{}", err.message());
    }

    fn with_policy(toml_policies: &str) -> Result<(), Error> {
        let config: Config = toml::from_str(toml_policies).unwrap();
        check(&config, Path::new(""))
    }

    #[test]
    fn a_policy_cannot_widen_the_toolsets() {
        let err = with_policy(
            r#"
            [tools]
            enabled_toolsets = ["web", "file"]
            [[gateway.chat_policies]]
            platform = "discord"
            chat_id = "555"
            toolsets = ["terminal"]
            "#,
        )
        .unwrap_err();
        assert_eq!(err.code(), &codes::CONFIG_INVALID);
        assert!(
            err.message()
                .contains("'terminal' is outside the profile's toolsets"),
            "{}",
            err.message()
        );
        with_policy(
            r#"
            [tools]
            enabled_toolsets = ["web", "file"]
            [[gateway.chat_policies]]
            platform = "discord"
            chat_id = "555"
            toolsets = ["web"]
            "#,
        )
        .unwrap();
    }

    #[test]
    fn two_policies_with_the_same_matchers_tie() {
        let err = with_policy(
            r#"
            [[gateway.chat_policies]]
            platform = "discord"
            chat_id = "555"
            model = "a"
            [[gateway.chat_policies]]
            platform = "discord"
            chat_id = "555"
            model = "b"
            "#,
        )
        .unwrap_err();
        assert!(
            err.message().contains("neither would win"),
            "{}",
            err.message()
        );
        let err = with_policy("[[gateway.chat_policies]]\nplatform = \"discord\"\n").unwrap_err();
        assert!(
            err.message()
                .contains("chat_id, thread_id, chat_type or user_id")
        );
    }

    #[test]
    fn a_disabled_skill_is_outside_the_policy_reach() {
        let err = with_policy(
            r#"
            [skills]
            disabled = ["deploy"]
            [[gateway.chat_policies]]
            platform = "telegram"
            chat_type = "group"
            skills = ["deploy"]
            "#,
        )
        .unwrap_err();
        assert!(
            err.message().contains("'deploy' is disabled"),
            "{}",
            err.message()
        );
    }

    #[test]
    fn an_enabled_health_export_needs_a_safe_endpoint() {
        let parse = |text: &str| check(&toml::from_str::<Config>(text).unwrap(), Path::new(""));
        assert!(parse("[observability.gateway_health]\nenabled = true\n").is_err());
        assert!(
            parse("[observability.gateway_health]\nenabled = true\nendpoint = \"http://collector.example/v1/metrics\"\n")
                .is_err()
        );
        parse("[observability.gateway_health]\nenabled = true\nendpoint = \"http://127.0.0.1:4318/v1/metrics\"\n").unwrap();
        parse("[observability.gateway_health]\nenabled = true\nendpoint = \"https://otel.example/v1/metrics\"\n").unwrap();
        parse("[observability.gateway_health]\nendpoint = \"http://x\"\n").unwrap();
    }
}
