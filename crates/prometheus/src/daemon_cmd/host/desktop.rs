//! The daemon side of the desktop's profile-scoped methods (spec:
//! desktop-app, Settings write validated config patches; Profile switching
//! is explicit; Bot chats are created atomically by the daemon).

use pmt_core::error::{Error, codes};
use pmt_rpc::contracts::{ConfigChange, ConfigReloaded};
use serde_json::Value;

use super::DaemonHost;

impl DaemonHost {
    /// Writes `changes` to `profile`'s `config.toml` in one validated
    /// write, then reloads it so the change applies at once.
    pub(super) async fn patch_config(
        &self,
        profile: &str,
        changes: Vec<ConfigChange>,
    ) -> Result<(), Error> {
        let profile = self.profile(profile)?;
        let edits = changes
            .into_iter()
            .map(|change| {
                let raw = change
                    .value
                    .map(|v| raw_value(&change.key, v))
                    .transpose()?;
                Ok((change.key, raw))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        profile.scope().profile().config_file().patch(&edits)?;
        self.check(&profile).await;
        Ok(())
    }

    /// Re-reads `profile`'s `config.toml` now and reports its state.
    pub(super) async fn reload_config(&self, name: &str) -> Result<ConfigReloaded, Error> {
        let profile = self.profile(name)?;
        self.check(&profile).await;
        let state = self
            .config_states()
            .into_iter()
            .find(|p| p.profile == name)
            .ok_or_else(|| {
                Error::new(
                    &codes::PROFILE_NOT_FOUND,
                    format!("profile '{name}' is not served"),
                )
            })?;
        Ok(ConfigReloaded {
            profile: state.profile,
            reloads: state.reloads,
            error: state.error,
            pending_restart: state.pending_restart,
        })
    }
}

/// The `config set` text of a JSON value: strings as they are, numbers
/// and booleans as literals, arrays of them as inline arrays (JSON's array
/// syntax is TOML's for scalars).
fn raw_value(key: &str, value: Value) -> Result<String, Error> {
    match value {
        Value::String(text) => Ok(text),
        Value::Number(_) | Value::Bool(_) => Ok(value.to_string()),
        Value::Array(items) if items.iter().all(is_scalar) => Ok(Value::Array(items).to_string()),
        Value::Null => Err(Error::new(&codes::CONFIG_INVALID_ARGUMENT, "null")),
        Value::Array(_) | Value::Object(_) => Err(Error::new(
            &codes::CONFIG_INVALID_ARGUMENT,
            format!("'{key}' takes a plain value; set the keys of a table one by one"),
        )),
    }
}

fn is_scalar(value: &Value) -> bool {
    matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::raw_value;

    #[test]
    fn json_values_become_config_set_text() {
        assert_eq!(raw_value("k", json!("gpt")).unwrap(), "gpt");
        assert_eq!(raw_value("k", json!(3)).unwrap(), "3");
        assert_eq!(raw_value("k", json!(true)).unwrap(), "true");
        assert_eq!(raw_value("k", json!(["a", "b"])).unwrap(), r#"["a","b"]"#);
        assert!(raw_value("k", json!({"a": 1})).is_err());
        assert!(raw_value("k", json!([{"a": 1}])).is_err());
    }
}
