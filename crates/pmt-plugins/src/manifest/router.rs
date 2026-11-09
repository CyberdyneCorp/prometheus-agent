//! `[delegation_router]`: the delegation router a `delegation-router`
//! plugin provides (spec: subagent-delegation, Delegation routers fail
//! closed).
//!
//! ```toml
//! [delegation_router]
//! name = "tiers"            # `delegation.router`; the plugin id when absent
//!
//! [delegation_router.params]
//! tier = { type = "string", enum = ["fast", "deep"] }
//! ```
//!
//! Each parameter is a typed task field the router adds to `delegate_task`
//! (its value is a JSON Schema); the host refuses a router whose
//! parameter names are not new snake-case fields when it is selected.

use pmt_core::error::Error;
use serde_json::{Map, Value};
use toml::Table;

use super::invalid;

/// The router a plugin declares.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouterDeclaration {
    /// The router's name (`None`: the plugin id).
    pub name: Option<String>,
    /// Task parameter name -> JSON Schema.
    pub params: Map<String, Value>,
}

const KEYS: &[&str] = &["name", "params"];

/// Parses `[delegation_router]` (absent = `None`).
///
/// # Errors
///
/// `plugins.manifest` naming an unknown key or a value of the wrong type.
pub(super) fn parse(value: Option<&toml::Value>) -> Result<Option<RouterDeclaration>, Error> {
    let table: &Table = match value {
        None => return Ok(None),
        Some(toml::Value::Table(t)) => t,
        Some(_) => return Err(invalid("delegation_router must be a table".to_owned())),
    };
    if let Some(key) = table.keys().find(|k| !KEYS.contains(&k.as_str())) {
        return Err(invalid(format!("unknown field 'delegation_router.{key}'")));
    }
    let name = match table.get("name") {
        None => None,
        Some(toml::Value::String(name)) if !name.is_empty() => Some(name.clone()),
        Some(_) => {
            return Err(invalid(
                "delegation_router.name must be a non-empty string".to_owned(),
            ));
        }
    };
    let params = match table.get("params") {
        None => Map::new(),
        Some(toml::Value::Table(params)) => params
            .iter()
            .map(|(key, schema)| match schema {
                toml::Value::Table(_) => serde_json::to_value(schema)
                    .map(|schema| (key.clone(), schema))
                    .map_err(|e| invalid(format!("delegation_router.params.{key}: {e}"))),
                _ => Err(invalid(format!(
                    "delegation_router.params.{key} must be a JSON Schema table"
                ))),
            })
            .collect::<Result<_, _>>()?,
        Some(_) => {
            return Err(invalid(
                "delegation_router.params must be a table".to_owned(),
            ));
        }
    };
    Ok(Some(RouterDeclaration { name, params }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str) -> Result<Option<RouterDeclaration>, Error> {
        let table: Table = text.parse().unwrap();
        parse(table.get("delegation_router"))
    }

    #[test]
    fn a_router_declares_its_name_and_typed_parameters() {
        let declared = read(
            "[delegation_router]\nname = \"tiers\"\n[delegation_router.params]\ntier = { type = \"string\" }\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(declared.name.as_deref(), Some("tiers"));
        assert_eq!(
            declared.params["tier"],
            serde_json::json!({"type": "string"})
        );
        assert_eq!(read("").unwrap(), None);
    }

    #[test]
    fn unknown_keys_and_untyped_parameters_are_refused() {
        assert!(read("[delegation_router]\nmodel = \"x\"\n").is_err());
        assert!(read("[delegation_router.params]\ntier = \"string\"\n").is_err());
    }
}
