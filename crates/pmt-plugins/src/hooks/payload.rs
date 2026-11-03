//! Versioned hook payloads (spec: plugin-system, Hook payloads are
//! versioned and additive).
//!
//! A payload is a JSON object carried in the WIT `hook-event` record with
//! `schema_version` = [`SCHEMA_VERSION`]. Within the hook world's major
//! version fields are only added, as optional fields, and each addition is
//! listed in [`ADDED`] with the minor version that introduced it; a plugin
//! built against an older minor receives only the fields its version
//! declares.

use serde_json::{Map, Value};

/// The payload schema of the hook world's current major version.
pub const SCHEMA_VERSION: &str = "prometheus.hooks.v1";

/// One field added within the major version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Added {
    /// The hook whose payload gained it.
    pub hook: &'static str,
    /// The field.
    pub field: &'static str,
    /// The hook world minor version that added it.
    pub minor: u64,
}

/// Fields added after hook world 1.0 (none yet).
pub const ADDED: &[Added] = &[];

/// One hook's payload.
#[derive(Debug, Clone, PartialEq)]
pub struct Payload {
    fields: Map<String, Value>,
}

impl Payload {
    /// `fields` plus `schema_version`.
    #[must_use]
    pub fn new(mut fields: Map<String, Value>) -> Self {
        fields.insert("schema_version".to_owned(), SCHEMA_VERSION.into());
        Self { fields }
    }

    /// The fields.
    #[must_use]
    pub fn fields(&self) -> &Map<String, Value> {
        &self.fields
    }

    /// The JSON a plugin built against hook world `1.<minor>` receives.
    #[must_use]
    pub fn for_minor(&self, hook: &str, minor: u64) -> String {
        self.for_minor_in(ADDED, hook, minor)
    }

    /// [`Self::for_minor`] against another addition table (tests).
    #[must_use]
    pub fn for_minor_in(&self, added: &[Added], hook: &str, minor: u64) -> String {
        let mut fields = self.fields.clone();
        for addition in added {
            if addition.hook == hook && addition.minor > minor {
                fields.remove(addition.field);
            }
        }
        Value::Object(fields).to_string()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn an_old_plugin_receives_only_the_fields_of_its_version() {
        // Spec scenario "Old plugin receives a new payload": host at 1.3
        // added `cost_usd` to `post_llm_call`.
        let added = [Added {
            hook: "post_llm_call",
            field: "cost_usd",
            minor: 3,
        }];
        let Value::Object(fields) = json!({"model": "m", "cost_usd": 0.5}) else {
            unreachable!()
        };
        let payload = Payload::new(fields);
        let old: Value =
            serde_json::from_str(&payload.for_minor_in(&added, "post_llm_call", 0)).unwrap();
        assert_eq!(
            old,
            json!({"model": "m", "schema_version": "prometheus.hooks.v1"})
        );
        let new: Value =
            serde_json::from_str(&payload.for_minor_in(&added, "post_llm_call", 3)).unwrap();
        assert_eq!(new["cost_usd"], 0.5);
        let other: Value =
            serde_json::from_str(&payload.for_minor_in(&added, "pre_llm_call", 0)).unwrap();
        assert_eq!(other["cost_usd"], 0.5);
    }
}
