//! What runs a plugin's contributions: a WASM instance in this process,
//! or an out-of-process host such as a python-bridge sidecar (spec:
//! python-bridge, Supported `PluginContext` subset). System-prompt sections,
//! slash commands and model-provider hooks reach their plugin only
//! through this trait, so every host's registrations follow the same
//! grant checks and reload rules.
//!
//! Every method blocks: call it off the async runtime. The outer `Err` is
//! a host failure (a trap, a timeout, a crashed sidecar), the inner one
//! the plugin's own answer.

use pmt_core::capability::CapabilitySet;
use pmt_core::error::Error;

use crate::host::surfaces_world::{ApprovalRequest, ApprovalResponse};
use crate::host::{CallInfo, Instance};

/// A plugin host's side of the contributions.
pub trait Backend: Send + Sync + std::fmt::Debug {
    /// The plugin id.
    fn id(&self) -> &str;

    /// What the plugin is granted now.
    fn granted(&self) -> CapabilitySet;

    /// The text of system-prompt section `id`.
    ///
    /// # Errors
    ///
    /// A host failure.
    fn render_section(&self, call: &CallInfo, id: &str) -> Result<Result<String, String>, Error>;

    /// Runs slash command `/<name> <args>`.
    ///
    /// # Errors
    ///
    /// A host failure.
    fn run_command(
        &self,
        call: &CallInfo,
        name: &str,
        args: &str,
    ) -> Result<Result<String, String>, Error>;

    /// Headers for a request to model provider `provider`.
    ///
    /// # Errors
    ///
    /// A host failure.
    fn model_headers(
        &self,
        call: &CallInfo,
        provider: &str,
    ) -> Result<Result<Vec<(String, String)>, String>, Error>;

    /// The JSON body of a request to `provider`, rewritten.
    ///
    /// # Errors
    ///
    /// A host failure.
    fn prepare_request(
        &self,
        call: &CallInfo,
        provider: &str,
        body: &str,
    ) -> Result<Result<String, String>, Error>;

    /// The models `provider` lists.
    ///
    /// # Errors
    ///
    /// A host failure.
    fn list_models(
        &self,
        call: &CallInfo,
        provider: &str,
    ) -> Result<Result<Vec<String>, String>, Error>;

    /// Sends one provider-object request document (the JSON shape of
    /// `category`) to provider `id` and returns its JSON answer.
    ///
    /// # Errors
    ///
    /// A host failure.
    fn invoke_provider(
        &self,
        call: &CallInfo,
        category: crate::providers::Category,
        id: &str,
        request: &str,
    ) -> Result<Result<String, String>, Error>;

    /// Presents `request` on approval transport `name`, waiting up to
    /// `bound`.
    ///
    /// # Errors
    ///
    /// A host failure.
    fn present_approval(
        &self,
        call: &CallInfo,
        name: &str,
        request: &ApprovalRequest,
        bound: std::time::Duration,
    ) -> Result<Result<ApprovalResponse, String>, Error>;
}

impl Backend for Instance {
    fn id(&self) -> &str {
        Instance::id(self)
    }

    fn granted(&self) -> CapabilitySet {
        self.context().grants.current()
    }

    fn render_section(&self, call: &CallInfo, id: &str) -> Result<Result<String, String>, Error> {
        Instance::render_section(self, call, id)
    }

    fn run_command(
        &self,
        call: &CallInfo,
        name: &str,
        args: &str,
    ) -> Result<Result<String, String>, Error> {
        Instance::run_command(self, call, name, args)
    }

    fn model_headers(
        &self,
        call: &CallInfo,
        provider: &str,
    ) -> Result<Result<Vec<(String, String)>, String>, Error> {
        Instance::model_headers(self, call, provider)
    }

    fn prepare_request(
        &self,
        call: &CallInfo,
        provider: &str,
        body: &str,
    ) -> Result<Result<String, String>, Error> {
        Instance::prepare_request(self, call, provider, body)
    }

    fn list_models(
        &self,
        call: &CallInfo,
        provider: &str,
    ) -> Result<Result<Vec<String>, String>, Error> {
        Instance::list_models(self, call, provider)
    }

    fn present_approval(
        &self,
        call: &CallInfo,
        name: &str,
        request: &ApprovalRequest,
        bound: std::time::Duration,
    ) -> Result<Result<ApprovalResponse, String>, Error> {
        Instance::present_approval(self, call, name, request, bound)
    }

    fn invoke_provider(
        &self,
        call: &CallInfo,
        _category: crate::providers::Category,
        id: &str,
        request: &str,
    ) -> Result<Result<String, String>, Error> {
        Instance::invoke_provider(self, call, id, request)
    }
}

/// Tools an out-of-process plugin host registers into a session's
/// registry (the python bridge); WASM plugins register through
/// [`crate::tools::register`].
pub trait ToolSource: Send + Sync + std::fmt::Debug {
    /// The plugin id.
    fn plugin(&self) -> &str;

    /// Registers the plugin's tools under its toolset; returns the
    /// registered names.
    fn register_tools(
        &self,
        registry: &mut pmt_tools::ToolRegistry,
        base: &CallInfo,
        calls: &crate::tools::SessionCalls,
    ) -> Vec<String>;
}
