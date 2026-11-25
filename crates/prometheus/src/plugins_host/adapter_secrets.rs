//! Secrets an adapter plugin stores (spec: platform-adapters, Google Chat
//! adapter: a user's `/setup-files` OAuth token goes to the secrets
//! store). The WIT `secrets` import only reads, so a plugin hands a value
//! to keep as a polled item `{"store_secret": {"name", "value"}}`; the
//! host writes it into the profile's secrets store only when the
//! descriptor lists the name in `writable_secrets` and the plugin's grants
//! let it read that secret anyway. Anything else is dropped with a
//! warning, and the item never reaches the gateway as a message.

use pmt_config::ProfileScope;
use pmt_core::capability::{Capability, CapabilitySet};
use pmt_core::secret::SecretString;
use serde_json::Value;

/// Where an adapter's `store_secret` items go.
pub(super) struct SecretWriter {
    scope: ProfileScope,
    plugin: String,
    names: Vec<String>,
}

impl SecretWriter {
    /// The writer of the names `descriptor` declares writable and
    /// `grants` lets the plugin read.
    pub(super) fn new(
        scope: &ProfileScope,
        plugin: &str,
        descriptor: &Value,
        grants: &CapabilitySet,
    ) -> Self {
        let names = descriptor["writable_secrets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|n| grants.allows(&Capability::SecretsRead((*n).to_owned())))
            .map(str::to_owned)
            .collect();
        Self {
            scope: scope.clone(),
            plugin: plugin.to_owned(),
            names,
        }
    }

    /// Stores the secret of a `store_secret` item; whether `item` was one
    /// (and so is no message).
    pub(super) fn consume(&self, item: &Value) -> bool {
        let Some(request) = item.get("store_secret") else {
            return false;
        };
        let (Some(name), Some(value)) = (request["name"].as_str(), request["value"].as_str())
        else {
            tracing::warn!(plugin = %self.plugin, "a malformed store_secret item was dropped");
            return true;
        };
        if !self.names.iter().any(|n| n == name) {
            tracing::warn!(
                plugin = %self.plugin,
                "refused to store secret {name}: not a writable secret the plugin may read"
            );
            return true;
        }
        if let Err(err) = self
            .scope
            .secrets()
            .store()
            .set(name, &SecretString::new(value))
        {
            tracing::warn!(plugin = %self.plugin, "cannot store secret {name}: {}", err.message());
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use pmt_config::{Home, Launch, Overrides, ProfileName};
    use serde_json::json;

    use super::*;

    #[test]
    fn only_writable_secrets_the_plugin_may_read_are_stored() {
        let root = tempfile::tempdir().unwrap();
        let home = Home::open(root.path()).unwrap();
        let profile = home.profile(&ProfileName::default_profile()).unwrap();
        let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap();
        let grants =
            CapabilitySet::parse(["secrets.read:USER_TOKENS", "secrets.read:BOT_TOKEN"]).unwrap();
        let descriptor = json!({"writable_secrets": ["USER_TOKENS", "NOT_GRANTED"]});
        let writer = SecretWriter::new(&scope, "kit", &descriptor, &grants);
        let store = |name: &str| json!({"store_secret": {"name": name, "value": "v"}});
        for name in ["USER_TOKENS", "NOT_GRANTED", "BOT_TOKEN"] {
            assert!(
                writer.consume(&store(name)),
                "a store item is never a message"
            );
        }
        assert!(!writer.consume(&json!({"id": "1", "text": "hi"})));
        let secrets = scope.secrets();
        assert_eq!(secrets.resolve("USER_TOKENS").unwrap().expose(), "v");
        // Declared but not granted, or granted but not declared writable.
        assert!(secrets.resolve("NOT_GRANTED").is_err());
        assert!(secrets.resolve("BOT_TOKEN").is_err());
    }
}
