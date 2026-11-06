//! `delegation-router` plugins (spec: subagent-delegation, Delegation
//! routers fail closed).
//!
//! A loaded plugin exporting the `delegation-router` world provides one
//! router, named by `[delegation_router] name` in its `plugin.toml` (the
//! plugin id when absent), with the typed task parameters the table's
//! `params` declares (parameter name -> JSON Schema). The host checks the
//! names when the router is selected; this module only reads them.

use std::sync::Arc;

use pmt_core::error::Error;
use serde_json::{Map, Value};

use crate::host::{CallInfo, Instance};
use crate::load::Loaded;

/// One plugin's delegation router.
#[derive(Debug, Clone)]
pub struct PluginRouter {
    /// Its name (`delegation.router`).
    pub name: String,
    /// The typed task parameters it adds.
    pub params: Map<String, Value>,
    instance: Arc<Instance>,
}

impl PluginRouter {
    /// Routes the JSON task: the decision JSON, or the plugin's refusal
    /// of that task as `Ok(Err(..))`.
    ///
    /// # Errors
    ///
    /// The call trapped or timed out.
    pub fn route(&self, call: &CallInfo, task: &str) -> Result<Result<String, String>, Error> {
        self.instance.route(call, task)
    }
}

/// The routers of `loaded`'s plugins, in load order.
#[must_use]
pub fn routers(loaded: &Loaded) -> Vec<PluginRouter> {
    loaded
        .plugins
        .iter()
        .filter(|p| p.instance.implements("delegation-router"))
        .map(|p| {
            let declared = p.manifest.delegation_router.as_ref();
            PluginRouter {
                name: declared
                    .and_then(|d| d.name.clone())
                    .unwrap_or_else(|| p.manifest.id.clone()),
                params: declared.map(|d| d.params.clone()).unwrap_or_default(),
                instance: Arc::clone(&p.instance),
            }
        })
        .collect()
}
