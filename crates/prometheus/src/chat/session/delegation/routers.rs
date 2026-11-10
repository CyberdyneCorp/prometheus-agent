//! Delegation routers of a session (spec: subagent-delegation, Delegation
//! routers fail closed): `default`, or the router an enabled
//! `delegation-router` plugin of the profile provides. The router is
//! selected once, when the session starts (or on `/delegation router
//! <name> --now`), from the plugin set loaded then; a name no loaded plugin
//! provides leaves the session without a router, so every spawn fails with
//! `delegation.router_unavailable` and nothing falls back to `default`.

use std::sync::Arc;

use pmt_agent::turn::model::BoxFuture;
use pmt_delegation::Refusal;
use pmt_delegation::contract::Task;
use pmt_delegation::router::{self, Route, Router};
use pmt_plugins::host::CallInfo;
use pmt_plugins::routers::PluginRouter;
use serde_json::{Map, Value};

/// The router `name` among those of `plugins`, for calls made as `call`.
///
/// # Errors
///
/// `delegation.router_unavailable` naming the router.
pub fn select(
    name: &str,
    plugins: &crate::plugins_host::ProfilePlugins,
    call: &CallInfo,
) -> Result<Arc<dyn Router>, Refusal> {
    let provided: Vec<Arc<dyn Router>> = pmt_plugins::routers::routers(&plugins.current())
        .into_iter()
        .map(|router| {
            Arc::new(Plugin {
                router,
                call: call.clone(),
            }) as Arc<dyn Router>
        })
        .collect();
    router::select(name, &provided)
}

/// A plugin's router.
struct Plugin {
    router: PluginRouter,
    call: CallInfo,
}

impl Router for Plugin {
    fn name(&self) -> &str {
        &self.router.name
    }

    fn params(&self) -> Map<String, Value> {
        self.router.params.clone()
    }

    fn route<'a>(&'a self, task: &'a Task) -> BoxFuture<'a, Result<Route, String>> {
        let router = self.router.clone();
        let call = self.call.clone();
        let text = task.to_json().to_string();
        Box::pin(async move {
            // A plugin call blocks its thread (its own CPU and time bounds
            // apply).
            let answer = pmt_core::spawn::blocking(move || router.route(&call, &text))
                .await
                .map_err(|e| format!("the router stopped: {e}"))?;
            match answer {
                Ok(Ok(decision)) => Route::from_json(&decision),
                Ok(Err(refusal)) => Err(refusal),
                // A trap or timeout refuses the task: the router never
                // decided, and nothing may fall back to the default.
                Err(err) => Err(format!("the router failed: {}", err.message())),
            }
        })
    }
}
