//! Creating, duplicating and deleting bots on this machine (spec:
//! bot-mode, Creating, duplicating and deleting bots). A bot is a
//! profile: created fresh, cloned from another profile's config, skills,
//! `SOUL.md` and memory, or with no bundled skills. Static API keys of
//! the source are copied unless the caller says not to; subscription
//! sign-ins (`OAUTH_*`) and gateway-owned secrets never are. The daemon
//! then serves the profile, creates its canonical Bot Chat and runs its
//! introduction turn.

use base64::Engine as _;
use pmt_config::profiles::CloneFrom;
use pmt_config::{Home, Launch, Overrides, ProfileDir, ProfileName, ProfileScope};
use pmt_core::error::{Error, codes};
use pmt_rpc::contracts::{BotAvatar, BotCreateParams};
use serde_json::{Map, Value};

use super::meta;

/// What a creation left to the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Made {
    /// The new profile.
    pub profile: String,
    /// The secrets copied from the source.
    pub copied_secrets: Vec<String>,
}

/// Whether secret `name` is a static API key a new bot may copy: a
/// `*_KEY` secret that is neither a subscription sign-in nor a secret the
/// gateway owns (bot tokens, dashboard credentials).
#[must_use]
pub fn copyable(name: &str, surface: &pmt_config::secrets::surface::SurfaceOnly) -> bool {
    name.ends_with("_KEY") && !name.starts_with("OAUTH_") && !surface.contains(name)
}

fn open(home: &Home, name: &ProfileName) -> Result<ProfileScope, Error> {
    ProfileScope::open(
        home.existing_profile(name)?,
        &Overrides::default(),
        Launch::Other,
    )
}

/// Creates the bot `params` describes in `home`; `asking` is the profile
/// that asked (the key source when nothing is cloned).
///
/// # Errors
///
/// `profile.*` for a bad or taken name or an unknown source; `config.*`
/// for a refused setting (nothing is left behind then).
pub fn create(home: &Home, asking: &str, params: &BotCreateParams) -> Result<Made, Error> {
    let source = params
        .clone_from
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(ProfileName::parse)
        .transpose()?;
    let profile = home.create_profile(&params.name, source.as_ref().map(CloneFrom::of))?;
    let built = (|| {
        if let Some(source) = &source {
            crate::profile_cmd::clone_memory(home, source, &profile)?;
        }
        if params.empty {
            pmt_skills::optout::set_included(&profile, false)?;
        }
        let key_source = source
            .clone()
            .map_or_else(|| ProfileName::parse(asking), Ok)?;
        settle(
            &profile,
            params,
            inherited_model(home, &key_source, source.is_none())?,
        )?;
        let copied = if params.copy_api_keys.unwrap_or(true) {
            copy_keys(home, &key_source, &profile)?
        } else {
            Vec::new()
        };
        let mut changes = Map::new();
        for (key, value) in [
            ("title", &params.title),
            ("description", &params.description),
        ] {
            if let Some(text) = value {
                changes.insert(key.to_owned(), Value::String(text.clone()));
            }
        }
        write_meta(home, &profile, &changes)?;
        Ok::<_, Error>(copied)
    })();
    match built {
        Ok(copied_secrets) => Ok(Made {
            profile: profile.name().to_string(),
            copied_secrets,
        }),
        Err(err) => {
            let _ = std::fs::remove_dir_all(profile.path());
            Err(err)
        }
    }
}

/// The model a fresh bot starts with: the asking profile's (a profile
/// without one cannot answer); a clone keeps its source's.
fn inherited_model(
    home: &Home,
    asking: &ProfileName,
    fresh: bool,
) -> Result<Option<(Option<String>, String)>, Error> {
    if !fresh {
        return Ok(None);
    }
    let config = open(home, asking)?.config().model.clone();
    Ok(config.default.map(|model| (config.provider, model)))
}

/// The new profile's `SOUL.md` and settings (model, toolsets, skills, MCP
/// servers), in one validated write.
fn settle(
    profile: &ProfileDir,
    params: &BotCreateParams,
    inherited: Option<(Option<String>, String)>,
) -> Result<(), Error> {
    if let Some(soul) = params.soul.as_deref().filter(|s| !s.trim().is_empty()) {
        let path = profile.path().join(pmt_config::profiles::IDENTITY_FILE);
        std::fs::write(&path, soul).map_err(|e| Error::io(&codes::HOME_IO, &path, &e))?;
    }
    let scope = ProfileScope::open(profile.clone(), &Overrides::default(), Launch::Other)?;
    let config = scope.config();
    let mut pairs: Vec<(String, String)> = Vec::new();
    let pinned = params.model.as_deref().filter(|m| !m.trim().is_empty());
    let model = match pinned {
        Some(model) => match model.split_once('/') {
            Some((provider, name)) => Some((Some(provider.to_owned()), name.to_owned())),
            None => Some((None, model.to_owned())),
        },
        None => inherited,
    };
    if let Some((provider, name)) = model {
        if let Some(provider) = provider {
            pairs.push(("model.provider".into(), provider));
        }
        pairs.push(("model.default".into(), name));
    }
    if !params.toolsets.is_empty() {
        pairs.push(("tools.enabled_toolsets".into(), toml_list(&params.toolsets)));
    }
    if !params.skills.is_empty() {
        let disabled: Vec<String> = config
            .skills
            .disabled
            .iter()
            .filter(|s| !params.skills.contains(s))
            .cloned()
            .collect();
        pairs.push(("skills.disabled".into(), toml_list(&disabled)));
    }
    if !params.mcp_servers.is_empty() {
        for name in config.mcp.servers.keys() {
            let on = params.mcp_servers.contains(name);
            pairs.push((format!("mcp.servers.{name}.enabled"), on.to_string()));
        }
    }
    if pairs.is_empty() {
        return Ok(());
    }
    let refs: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    profile.config_file().set_all(&refs)
}

fn toml_list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|i| format!("{i:?}")).collect();
    format!("[{}]", quoted.join(", "))
}

/// Copies `source`'s static API keys into `target`; their names.
fn copy_keys(home: &Home, source: &ProfileName, target: &ProfileDir) -> Result<Vec<String>, Error> {
    let from = open(home, source)?;
    let to = ProfileScope::open(target.clone(), &Overrides::default(), Launch::Other)?;
    let surface = pmt_config::secrets::surface::SurfaceOnly::of(from.config());
    let (from_store, to_store) = (from.secrets().store(), to.secrets().store());
    let mut copied = Vec::new();
    for name in from_store.names()? {
        if !copyable(&name, &surface) {
            continue;
        }
        if let Some(value) = from_store.get(&name)? {
            to_store.set(&name, &value)?;
            copied.push(name);
        }
    }
    Ok(copied)
}

fn write_meta(
    home: &Home,
    profile: &ProfileDir,
    changes: &Map<String, Value>,
) -> Result<(), Error> {
    if changes.is_empty() {
        return Ok(());
    }
    let scope = open(home, profile.name())?;
    let store = pmt_store::Store::open(&scope)?;
    meta::update(&store, changes, &|_| {
        Err(Error::new(
            &codes::RPC_INVALID_PARAMS,
            "no image generation here",
        ))
    })
    .map(drop)
}

/// Duplicates bot `source` as `name`: config, skills, `SOUL.md`, memory
/// and metadata (never its secrets beyond its static API keys, never its
/// sessions).
///
/// # Errors
///
/// As [`create`].
pub fn duplicate(home: &Home, source: &str, name: &str) -> Result<Made, Error> {
    let params = BotCreateParams {
        name: name.to_owned(),
        clone_from: Some(source.to_owned()),
        ..BotCreateParams::default()
    };
    let made = create(home, source, &params)?;
    let new = ProfileName::parse(&made.profile)?;
    let copied = (|| {
        let from = pmt_store::Store::open(&open(home, &ProfileName::parse(source)?)?)?;
        let changes = copied_meta(&from)?;
        write_meta(home, &home.existing_profile(&new)?, &changes)
    })();
    if let Err(err) = copied {
        if let Ok(profile) = home.existing_profile(&new) {
            let _ = std::fs::remove_dir_all(profile.path());
        }
        return Err(err);
    }
    Ok(made)
}

/// `from`'s metadata as `bot.meta.update` changes for a copy. Blobs are
/// per profile, so an image avatar travels as its bytes and the copy
/// stores them in its own blobs.
fn copied_meta(from: &pmt_store::Store) -> Result<Map<String, Value>, Error> {
    let mut copied = meta::read(from)?;
    copied.groups.clear();
    if let BotAvatar::Image {
        blob: Some(blob),
        mime,
        ..
    } = &copied.avatar
    {
        let bytes = meta::avatar_bytes(from, blob)?;
        copied.avatar = BotAvatar::Image {
            blob: None,
            mime: mime.clone(),
            data: Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
        };
    }
    let value = serde_json::to_value(&copied)
        .map_err(|e| Error::new(&codes::RPC_INTERNAL, format!("bot metadata: {e}")))?;
    let mut changes = value.as_object().cloned().unwrap_or_default();
    changes.remove("groups");
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: OAuth not copied (the rule; the daemon test runs it).
    #[test]
    fn only_static_api_keys_are_copied() {
        let surface =
            pmt_config::secrets::surface::SurfaceOnly::of(&pmt_config::schema::Config::default());
        assert!(copyable("OPENROUTER_API_KEY", &surface));
        assert!(!copyable("OAUTH_ANTHROPIC_DEFAULT_ACCESS", &surface));
        assert!(!copyable("OAUTH_ANTHROPIC_DEFAULT_META", &surface));
        assert!(!copyable("TELEGRAM_BOT_TOKEN", &surface));
        assert!(!copyable("GITHUB_TOKEN", &surface));
    }
}
