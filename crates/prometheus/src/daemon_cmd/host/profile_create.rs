//! The profile builder's creation (`pmt_web::DashboardHost::create_profile`,
//! spec: web-dashboard, Profile builder): `profile create` (optionally
//! cloning another profile's configuration without its secrets, sessions
//! or memories), the persona, then one validated config patch; when the
//! patch is refused the new profile is removed again, so a failed build
//! leaves nothing behind. The daemon serves the profile right away.

use pmt_config::profiles::CloneFrom;
use pmt_config::{Home, ProfileName};
use pmt_core::error::{Error, codes};
use serde_json::{Value, json};

fn texts(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn toml_list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|i| format!("{i:?}")).collect();
    format!("[{}]", quoted.join(", "))
}

/// The config patch the builder's steps describe.
fn patch(body: &Value) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for (key, field) in [("model.provider", "provider"), ("model.default", "model")] {
        if let Some(value) = body[field].as_str().filter(|v| !v.trim().is_empty()) {
            pairs.push((key.to_owned(), value.trim().to_owned()));
        }
    }
    let toolsets = texts(&body["toolsets"]);
    if !toolsets.is_empty() {
        pairs.push(("tools.enabled_toolsets".to_owned(), toml_list(&toolsets)));
    }
    let disabled = texts(&body["skills_disabled"]);
    if !disabled.is_empty() {
        pairs.push(("skills.disabled".to_owned(), toml_list(&disabled)));
    }
    for adapter in texts(&body["adapters"]) {
        pairs.push((
            format!("gateway.platforms.{adapter}.enabled"),
            "true".to_owned(),
        ));
    }
    pairs
}

/// Creates the profile `body` describes in `home`; its name.
///
/// # Errors
///
/// `profile.*` for a bad or taken name or an unknown clone source,
/// `config.*` for a refused patch (nothing is left behind then).
pub(super) fn create(home: &Home, body: &Value) -> Result<Value, Error> {
    let name = body["name"]
        .as_str()
        .ok_or_else(|| Error::new(&codes::CLI_USAGE, "the profile needs a name"))?;
    let source = body["clone_from"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(ProfileName::parse)
        .transpose()?;
    let clone = source.as_ref().map(CloneFrom::of);
    let profile = home.create_profile(name, clone)?;
    let built = (|| {
        // A clone copies configuration only: no memory files either.
        let _ = std::fs::remove_dir_all(profile.path().join("memories"));
        if let Some(text) = body["description"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
        {
            let path = profile.path().join(pmt_config::profiles::DESCRIPTION_FILE);
            pmt_core::private_fs::write_atomic(&path, text.trim().as_bytes())
                .map_err(|e| Error::io(&codes::HOME_IO, &path, &e))?;
        }
        if let Some(soul) = body["soul"].as_str().filter(|s| !s.trim().is_empty()) {
            std::fs::write(
                profile.path().join(pmt_config::profiles::IDENTITY_FILE),
                soul,
            )
            .map_err(|e| Error::new(&codes::HOME_IO, format!("SOUL.md: {e}")))?;
        }
        let pairs = patch(body);
        if !pairs.is_empty() {
            let refs: Vec<(&str, &str)> = pairs
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            profile.config_file().set_all(&refs)?;
        }
        Ok::<_, Error>(())
    })();
    if let Err(err) = built {
        let _ = std::fs::remove_dir_all(profile.path());
        return Err(err);
    }
    Ok(json!({"name": name, "cloned_from": source.map(|s| s.as_str().to_owned())}))
}
