//! Delegation routers (spec: subagent-delegation, Delegation routers fail
//! closed).
//!
//! `delegation.router` names the router: `default` (built in) or one a
//! plugin provides. A router may declare typed task parameters and choose,
//! per task, a model, a profile or a remote backend; it may also refuse a
//! single task, which then ends `refused` while its siblings run. A router
//! that is unknown or unavailable makes every spawn fail with
//! `delegation.router_unavailable`; there is no fallback to `default`.

use std::sync::{Arc, LazyLock};

use pmt_agent::turn::model::BoxFuture;
use pmt_core::error::codes;
use regex::Regex;
use serde_json::{Map, Value};

use crate::Refusal;
use crate::contract::{TASK_FIELDS, Task};

/// The built-in router's name.
pub const DEFAULT: &str = "default";

static PARAM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9_]{0,63}$").expect("param pattern"));

/// What a router decided for one task.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Route {
    /// The model (`provider/model`); `None` leaves the normal choice.
    pub model: Option<String>,
    /// The profile the child runs in.
    pub profile: Option<String>,
    /// A remote backend for the child's terminal.
    pub backend: Option<String>,
}

impl Route {
    /// Reads a router's JSON decision.
    ///
    /// # Errors
    ///
    /// The decision is not an object of optional strings.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let value: Value =
            serde_json::from_str(text).map_err(|e| format!("decision is not JSON: {e}"))?;
        let object = value.as_object().ok_or("decision is not an object")?;
        let field = |key: &str| match object.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(format!("decision field '{key}' must be a string")),
        };
        Ok(Self {
            model: field("model")?,
            profile: field("profile")?,
            backend: field("backend")?,
        })
    }
}

/// A delegation router.
pub trait Router: Send + Sync {
    /// Its name (`delegation.router`).
    fn name(&self) -> &str;

    /// The typed task parameters it adds: name -> JSON Schema.
    fn params(&self) -> Map<String, Value> {
        Map::new()
    }

    /// Routes `task`; an `Err` refuses only that task, with the message.
    fn route<'a>(&'a self, task: &'a Task) -> BoxFuture<'a, Result<Route, String>>;
}

/// The built-in router: every task takes the normal model choice in the
/// launching profile.
#[derive(Debug, Default)]
pub struct DefaultRouter;

impl Router for DefaultRouter {
    fn name(&self) -> &str {
        DEFAULT
    }

    fn route<'a>(&'a self, _: &'a Task) -> BoxFuture<'a, Result<Route, String>> {
        Box::pin(async { Ok(Route::default()) })
    }
}

/// The unavailable-router refusal.
#[must_use]
pub fn unavailable(name: &str, why: &str) -> Refusal {
    Refusal::new(
        &codes::DELEGATION_ROUTER_UNAVAILABLE,
        format!("Delegation router '{name}' is unavailable ({why}); no subagent was spawned."),
    )
}

/// The router named `name` among `provided` (the enabled plugins'
/// routers), with its parameters checked.
///
/// # Errors
///
/// `delegation.router_unavailable` when no router has that name or its
/// parameters are malformed.
pub fn select(name: &str, provided: &[Arc<dyn Router>]) -> Result<Arc<dyn Router>, Refusal> {
    if name == DEFAULT {
        return Ok(Arc::new(DefaultRouter));
    }
    let router = provided
        .iter()
        .find(|r| r.name() == name)
        .cloned()
        .ok_or_else(|| unavailable(name, "no enabled plugin provides it"))?;
    for param in router.params().keys() {
        if !PARAM.is_match(param) || TASK_FIELDS.contains(&param.as_str()) {
            return Err(unavailable(
                name,
                &format!("its task parameter '{param}' is not a valid new field name"),
            ));
        }
    }
    Ok(router)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tiers(Map<String, Value>);

    impl Router for Tiers {
        fn name(&self) -> &'static str {
            "tiers"
        }

        fn params(&self) -> Map<String, Value> {
            self.0.clone()
        }

        fn route<'a>(&'a self, _: &'a Task) -> BoxFuture<'a, Result<Route, String>> {
            Box::pin(async { Ok(Route::default()) })
        }
    }

    /// Scenario: Missing router.
    #[test]
    fn a_router_no_plugin_provides_is_unavailable_and_never_falls_back() {
        let err = select("tiers", &[]).err().unwrap();
        assert_eq!(err.code.code, "delegation.router_unavailable");
        assert!(err.message.contains("'tiers'"));
        assert_eq!(select("default", &[]).unwrap().name(), "default");
    }

    #[test]
    fn router_params_must_be_new_snake_case_fields() {
        let ok: Arc<dyn Router> = Arc::new(Tiers(Map::from_iter([("tier".into(), Value::Null)])));
        assert!(select("tiers", &[ok]).is_ok());
        for bad in ["goal", "Tier", "9x"] {
            let r: Arc<dyn Router> = Arc::new(Tiers(Map::from_iter([(bad.into(), Value::Null)])));
            assert!(select("tiers", &[r]).is_err(), "{bad}");
        }
    }

    #[test]
    fn decisions_parse() {
        let route = Route::from_json(r#"{"model":"mock/fast","profile":null}"#).unwrap();
        assert_eq!(route.model.as_deref(), Some("mock/fast"));
        assert!(Route::from_json("[1]").is_err());
    }
}
