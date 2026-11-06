//! Calls into the `delegation-router` world (spec: subagent-delegation,
//! Delegation routers fail closed).

use pmt_core::error::Error;

use super::{CallInfo, Instance};

impl Instance {
    /// `plugin.route(task)`: the plugin's own `Err` (a refused task) is
    /// returned as `Ok(Err(text))`, a trap or timeout as `Err`.
    ///
    /// # Errors
    ///
    /// `plugins.load` when the component does not export the
    /// delegation-router world; `plugins.call` when the call traps or
    /// times out.
    pub fn route(&self, call: &CallInfo, task: &str) -> Result<Result<String, String>, Error> {
        let indices = self
            .router
            .as_ref()
            .ok_or_else(|| self.missing("delegation-router"))?;
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_route(&mut *store, task)
        })
    }
}
