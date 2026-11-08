//! The shared AWS profile files (`~/.aws/credentials`, `~/.aws/config`)
//! and the IAM Identity Center (SSO) token cache the AWS CLI writes on
//! `aws sso login`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use sha1::{Digest, Sha1};

/// One profile's settings, the credentials file's entries winning.
pub(crate) type Settings = BTreeMap<String, String>;

/// The sections of an INI file by name.
fn sections(text: &str) -> BTreeMap<String, Settings> {
    let mut out: BTreeMap<String, Settings> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let name = name.trim().to_owned();
            out.entry(name.clone()).or_default();
            current = Some(name);
            continue;
        }
        let (Some(section), Some((key, value))) = (&current, line.split_once('=')) else {
            continue;
        };
        out.entry(section.clone())
            .or_default()
            .insert(key.trim().to_owned(), value.trim().to_owned());
    }
    out
}

fn read(path: &Path) -> BTreeMap<String, Settings> {
    std::fs::read_to_string(path)
        .map(|text| sections(&text))
        .unwrap_or_default()
}

/// The AWS CLI's directory under the user's home.
pub(crate) fn aws_dir(home: &Path) -> PathBuf {
    home.join(".aws")
}

/// The settings of `profile` from both files (`[name]` in the credentials
/// file, `[profile name]` or `[default]` in the config file), and the
/// `[sso-session <name>]` section it names, if any.
pub(crate) fn load(home: &Path, profile: &str) -> Option<(Settings, Option<Settings>)> {
    let dir = aws_dir(home);
    let config = read(&dir.join("config"));
    let credentials = read(&dir.join("credentials"));
    let config_name = if profile == "default" {
        "default".to_owned()
    } else {
        format!("profile {profile}")
    };
    let mut settings = config.get(&config_name).cloned();
    if let Some(keys) = credentials.get(profile) {
        settings.get_or_insert_default().extend(keys.clone());
    }
    let settings = settings?;
    let session = settings
        .get("sso_session")
        .and_then(|name| config.get(&format!("sso-session {name}")))
        .cloned();
    Some((settings, session))
}

/// An SSO sign-in to exchange for role credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sso {
    pub region: String,
    pub account_id: String,
    pub role_name: String,
    /// The cache file the AWS CLI keeps the access token in.
    pub cache_file: PathBuf,
}

/// The SSO sign-in `settings` describe (a `sso_session` or the legacy
/// `sso_start_url` form), if complete.
pub(crate) fn sso(home: &Path, settings: &Settings, session: Option<&Settings>) -> Option<Sso> {
    let account_id = settings.get("sso_account_id")?.clone();
    let role_name = settings.get("sso_role_name")?.clone();
    // The cache is keyed by the session name, or by the start URL of a
    // legacy profile.
    let (key, region) = match (settings.get("sso_session"), session) {
        (Some(name), Some(session)) => (name.clone(), session.get("sso_region")?.clone()),
        _ => (
            settings.get("sso_start_url")?.clone(),
            settings.get("sso_region")?.clone(),
        ),
    };
    let digest = Sha1::digest(key.as_bytes());
    let name = format!("{}.json", crate::request::hex(&digest));
    Some(Sso {
        region,
        account_id,
        role_name,
        cache_file: aws_dir(home).join("sso").join("cache").join(name),
    })
}

/// The cached SSO access token and whether it is still valid at `now`.
pub(crate) fn sso_token(sso: &Sso, now: Timestamp) -> Option<(String, bool)> {
    let text = std::fs::read_to_string(&sso.cache_file).ok()?;
    let cache: serde_json::Value = serde_json::from_str(&text).ok()?;
    let token = cache["accessToken"].as_str()?.to_owned();
    let valid = cache["expiresAt"]
        .as_str()
        .and_then(|at| at.replace("UTC", "Z").parse::<Timestamp>().ok())
        .is_some_and(|at| at > now);
    Some((token, valid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_merge_both_files_and_find_their_sso_session() {
        let home = tempfile::tempdir().unwrap();
        let dir = aws_dir(home.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config"),
            "[profile dev]\nsso_session = corp\nsso_account_id = 111122223333\n\
             sso_role_name = Dev\n# comment\n[sso-session corp]\nsso_region = eu-west-1\n\
             sso_start_url = https://corp.awsapps.com/start\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("credentials"),
            "[default]\naws_access_key_id = AKID\naws_secret_access_key = s\n",
        )
        .unwrap();
        let (default, none) = load(home.path(), "default").unwrap();
        assert_eq!(default["aws_access_key_id"], "AKID");
        assert!(none.is_none());
        let (dev, session) = load(home.path(), "dev").unwrap();
        let sso = sso(home.path(), &dev, session.as_ref()).unwrap();
        assert_eq!(sso.region, "eu-west-1");
        // The AWS CLI names the cache file after the SHA-1 of the session.
        assert_eq!(
            sso.cache_file,
            dir.join("sso")
                .join("cache")
                .join("ee0bfd2552fbd840c02cc48b6e823320543c450f.json")
        );
        assert!(load(home.path(), "missing").is_none());
    }
}
