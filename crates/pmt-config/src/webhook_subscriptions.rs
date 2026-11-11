//! Webhook routes created with `prometheus webhooks subscribe` (spec: cli,
//! Webhook subscriptions): profile state, not configuration. They live in
//! `<profile>/webhooks.toml` beside `config.toml` (never in it), each with
//! the same fields as a `[gateway.platforms.webhook.routes.<name>]` table,
//! and the webhook adapter serves them next to the configured routes,
//! picking up changes without a restart.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

use crate::schema::WebhookRoute;

/// The file, in the profile directory.
pub const FILE: &str = "webhooks.toml";

/// One subscribed route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subscription {
    /// When it was created (RFC 3339).
    pub created_at: String,
    /// Its signing secret was generated for it (removed with the route).
    #[serde(default)]
    pub generated_secret: bool,
    /// The route.
    pub route: WebhookRoute,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    #[serde(default)]
    subscriptions: BTreeMap<String, Subscription>,
}

/// The file of the profile in `profile_dir`.
#[must_use]
pub fn path(profile_dir: &Path) -> PathBuf {
    profile_dir.join(FILE)
}

/// The profile's subscriptions by route name (none without the file).
///
/// # Errors
///
/// `config.parse` for a file that does not parse; `home.io`.
pub fn load(profile_dir: &Path) -> Result<BTreeMap<String, Subscription>, Error> {
    let path = path(profile_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => {
            return Err(Error::new(
                &codes::HOME_IO,
                format!("cannot read {}: {e}", path.display()),
            ));
        }
    };
    let document: Document = toml::from_str(&text).map_err(|e| {
        Error::new(
            &codes::CONFIG_PARSE,
            format!("{}: {}", path.display(), e.message()),
        )
    })?;
    Ok(document.subscriptions)
}

/// Replaces the profile's subscriptions (written atomically, owner-only).
///
/// # Errors
///
/// `home.io`.
pub fn save(
    profile_dir: &Path,
    subscriptions: &BTreeMap<String, Subscription>,
) -> Result<(), Error> {
    let path = path(profile_dir);
    let document = Document {
        subscriptions: subscriptions.clone(),
    };
    let text = toml::to_string(&document).unwrap_or_default();
    pmt_core::private_fs::write_atomic(&path, text.as_bytes()).map_err(|e| {
        Error::new(
            &codes::HOME_IO,
            format!("cannot write {}: {e}", path.display()),
        )
    })
}

/// The secret name a subscription's generated signing secret is stored
/// under: `WEBHOOK_<NAME>` (`-` becomes `_`).
#[must_use]
pub fn secret_name(route: &str) -> String {
    format!("WEBHOOK_{}", route.to_ascii_uppercase().replace('-', "_"))
}

/// A new signing secret: 32 random bytes as hex.
///
/// # Panics
///
/// When the OS random generator is unavailable.
#[must_use]
pub fn generate_secret() -> pmt_core::secret::SecretString {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the OS random generator is available");
    let hex = bytes.iter().fold(String::new(), |mut hex, b| {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
        hex
    });
    pmt_core::secret::SecretString::new(hex)
}

/// The subscribed routes the adapter serves; a file that does not parse
/// serves none (logged).
#[must_use]
pub fn routes(profile_dir: &Path) -> BTreeMap<String, WebhookRoute> {
    match load(profile_dir) {
        Ok(subscriptions) => subscriptions
            .into_iter()
            .map(|(name, s)| (name, s.route))
            .collect(),
        Err(err) => {
            tracing::warn!(error = %err, "webhook subscriptions ignored");
            BTreeMap::new()
        }
    }
}

/// What changes when the file does (its modification time and length),
/// `None` without the file.
#[must_use]
pub fn stamp(profile_dir: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path(profile_dir)).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscriptions_round_trip_and_a_missing_file_is_empty() {
        let dir =
            tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")).unwrap();
        assert!(load(dir.path()).unwrap().is_empty());
        let mut subs = BTreeMap::new();
        subs.insert(
            "deploys".to_owned(),
            Subscription {
                created_at: "2026-01-01T00:00:00Z".into(),
                generated_secret: true,
                route: WebhookRoute {
                    events: vec!["push".into()],
                    deliver: Some("telegram:123".into()),
                    ..WebhookRoute::default()
                },
            },
        );
        save(dir.path(), &subs).unwrap();
        assert_eq!(load(dir.path()).unwrap(), subs);
        assert_eq!(routes(dir.path())["deploys"].events, ["push"]);
        assert!(stamp(dir.path()).is_some());
        assert_eq!(secret_name("ci-deploys"), "WEBHOOK_CI_DEPLOYS");
        assert_eq!(generate_secret().expose().len(), 64);
    }
}
