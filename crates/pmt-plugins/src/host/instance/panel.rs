//! Calls into the `ui-panel` world (spec: web-dashboard, Dashboard plugin
//! contributions): a dashboard plugin's backend routes reach it as
//! `on-action` calls whose JSON describes the HTTP request.

use pmt_core::error::Error;

use super::{CallInfo, Instance};

impl Instance {
    /// `plugin.on-action(action)`: the plugin's own `Err` is returned as
    /// `Ok(Err(text))`, a trap or timeout as `Err`.
    ///
    /// # Errors
    ///
    /// `plugins.load` when the component does not export the ui-panel
    /// world; `plugins.call` when the call traps or times out.
    pub fn panel_action(
        &self,
        call: &CallInfo,
        action: &str,
    ) -> Result<Result<String, String>, Error> {
        let indices = self
            .panel
            .as_ref()
            .ok_or_else(|| self.missing("ui-panel"))?;
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_on_action(&mut *store, action)
        })
    }
}
