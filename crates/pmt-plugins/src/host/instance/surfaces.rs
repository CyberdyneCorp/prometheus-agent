//! Calls into the hook world's `surfaces` (1.2: stream holding, CLI
//! commands, approval transports) and the provider world's `models` (1.1:
//! model providers). A component built against an older minor exports
//! neither; the calls then answer as if it registered nothing.

use std::time::Duration;

use pmt_core::error::Error;

use super::super::bindings::exports::prometheus::hook;
use super::{CallInfo, Instance};

use hook::surfaces::{ApprovalRequest, ApprovalResponse, CliCommand, CliOutput, Terminal};

impl Instance {
    /// Whether the component exports the hook world's `surfaces` (1.2).
    #[must_use]
    pub fn has_surfaces(&self) -> bool {
        self.surfaces.is_some()
    }

    /// `surfaces.hold-stream()`; empty without `surfaces`.
    ///
    /// # Errors
    ///
    /// `plugins.call` when the call traps, overruns or the plugin is
    /// suspended.
    pub fn hold_stream(&self, call: &CallInfo) -> Result<Vec<String>, Error> {
        let Some(indices) = &self.surfaces else {
            return Ok(Vec::new());
        };
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_hold_stream(&mut *store)
        })
    }

    /// `surfaces.cli-commands()`; empty without `surfaces`.
    ///
    /// # Errors
    ///
    /// As [`Self::hold_stream`].
    pub fn cli_commands(&self, call: &CallInfo) -> Result<Vec<CliCommand>, Error> {
        let Some(indices) = &self.surfaces else {
            return Ok(Vec::new());
        };
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_cli_commands(&mut *store)
        })
    }

    /// `surfaces.run-cli(name, args, terminal)`.
    ///
    /// # Errors
    ///
    /// As [`Self::hold_stream`]; `plugins.load` without `surfaces`.
    pub fn run_cli(
        &self,
        call: &CallInfo,
        name: &str,
        args: &[String],
        terminal: Terminal,
    ) -> Result<Result<CliOutput, String>, Error> {
        let indices = self
            .surfaces
            .as_ref()
            .ok_or_else(|| self.missing("hook@1.2"))?;
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_run_cli(&mut *store, name, args, terminal)
        })
    }

    /// `surfaces.transports()`; empty without `surfaces`.
    ///
    /// # Errors
    ///
    /// As [`Self::hold_stream`].
    pub fn transports(&self, call: &CallInfo) -> Result<Vec<String>, Error> {
        let Some(indices) = &self.surfaces else {
            return Ok(Vec::new());
        };
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_transports(&mut *store)
        })
    }

    /// `surfaces.present-approval(name, request)`, bounded by `bound`.
    ///
    /// # Errors
    ///
    /// As [`Self::run_cli`].
    pub fn present_approval(
        &self,
        call: &CallInfo,
        name: &str,
        request: &ApprovalRequest,
        bound: Duration,
    ) -> Result<Result<ApprovalResponse, String>, Error> {
        let indices = self
            .surfaces
            .as_ref()
            .ok_or_else(|| self.missing("hook@1.2"))?;
        self.with_live(call, Some(bound), |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_present_approval(&mut *store, name, request)
        })
    }

    /// `models.descriptors()`; empty without `models`.
    ///
    /// # Errors
    ///
    /// As [`Self::hold_stream`].
    pub fn model_descriptors(&self, call: &CallInfo) -> Result<Vec<String>, Error> {
        let Some(indices) = &self.models else {
            return Ok(Vec::new());
        };
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_descriptors(&mut *store)
        })
    }

    /// `models.headers(provider)`; no headers without `models`.
    ///
    /// # Errors
    ///
    /// As [`Self::hold_stream`].
    pub fn model_headers(
        &self,
        call: &CallInfo,
        id: &str,
    ) -> Result<Result<Vec<(String, String)>, String>, Error> {
        let Some(indices) = &self.models else {
            return Ok(Ok(Vec::new()));
        };
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_headers(&mut *store, id)
        })
    }

    /// `models.prepare-request(provider, body)`; the body unchanged
    /// without `models`.
    ///
    /// # Errors
    ///
    /// As [`Self::hold_stream`].
    pub fn prepare_request(
        &self,
        call: &CallInfo,
        id: &str,
        body: &str,
    ) -> Result<Result<String, String>, Error> {
        let Some(indices) = &self.models else {
            return Ok(Ok(body.to_owned()));
        };
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_prepare_request(&mut *store, id, body)
        })
    }

    /// `models.list-models(provider)`.
    ///
    /// # Errors
    ///
    /// As [`Self::run_cli`], for `models`.
    pub fn list_models(
        &self,
        call: &CallInfo,
        id: &str,
    ) -> Result<Result<Vec<String>, String>, Error> {
        let indices = self
            .models
            .as_ref()
            .ok_or_else(|| self.missing("provider@1.1"))?;
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_list_models(&mut *store, id)
        })
    }
}
