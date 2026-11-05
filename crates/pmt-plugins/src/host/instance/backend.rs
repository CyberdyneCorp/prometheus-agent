//! Calls into the `terminal-backend` world (1.1: the typed backend
//! contract). A 1.0 component exports none of them and is refused when its
//! backend is read ([`crate::terminal::PluginBackend::read`]).

use pmt_core::error::Error;

use super::super::terminal_backend_world::{Execution, Request};
use super::{CallInfo, Instance};

impl Instance {
    /// `plugin.name()` and `plugin.descriptor()`.
    ///
    /// # Errors
    ///
    /// `plugins.load` without the 1.1 interface; `plugins.call` when a call
    /// traps or overruns.
    pub fn backend_identity(&self, call: &CallInfo) -> Result<(String, String), Error> {
        let indices = self
            .backend
            .as_ref()
            .ok_or_else(|| self.missing("terminal-backend@1.1"))?;
        self.with_live(call, None, |store, instance| {
            let guest = indices.load(&mut *store, instance)?;
            let name = guest.call_name(&mut *store)?;
            let descriptor = guest.call_descriptor(&mut *store)?;
            Ok((name, descriptor))
        })
    }

    /// `plugin.check()`.
    ///
    /// # Errors
    ///
    /// As [`Self::backend_identity`].
    pub fn backend_check(&self, call: &CallInfo) -> Result<Result<String, String>, Error> {
        self.backend_call(call, |guest, store| guest.call_check(store))
    }

    /// `plugin.execute(request)`.
    ///
    /// # Errors
    ///
    /// As [`Self::backend_identity`].
    pub fn backend_execute(
        &self,
        call: &CallInfo,
        request: &Request,
    ) -> Result<Result<Execution, String>, Error> {
        self.backend_call(call, |guest, store| guest.call_execute(store, request))
    }

    /// `plugin.upload(path, contents)`.
    ///
    /// # Errors
    ///
    /// As [`Self::backend_identity`].
    pub fn backend_upload(
        &self,
        call: &CallInfo,
        path: &str,
        contents: &[u8],
    ) -> Result<Result<(), String>, Error> {
        self.backend_call(call, |guest, store| {
            guest.call_upload(store, path, contents)
        })
    }

    /// `plugin.download(path)`.
    ///
    /// # Errors
    ///
    /// As [`Self::backend_identity`].
    pub fn backend_download(
        &self,
        call: &CallInfo,
        path: &str,
    ) -> Result<Result<Vec<u8>, String>, Error> {
        self.backend_call(call, |guest, store| guest.call_download(store, path))
    }

    /// `plugin.cancel(id)`.
    ///
    /// # Errors
    ///
    /// As [`Self::backend_identity`].
    pub fn backend_cancel(&self, call: &CallInfo, id: &str) -> Result<(), Error> {
        self.backend_call(call, |guest, store| guest.call_cancel(store, id))
    }

    /// `plugin.teardown()`.
    ///
    /// # Errors
    ///
    /// As [`Self::backend_identity`].
    pub fn backend_teardown(&self, call: &CallInfo) -> Result<(), Error> {
        self.backend_call(call, |guest, store| guest.call_teardown(store))
    }

    fn backend_call<R>(
        &self,
        call: &CallInfo,
        f: impl FnOnce(
            &super::super::terminal_backend_world::Guest,
            &mut wasmtime::Store<super::super::imports::HostState>,
        ) -> wasmtime::Result<R>,
    ) -> Result<R, Error> {
        let indices = self
            .backend
            .as_ref()
            .ok_or_else(|| self.missing("terminal-backend@1.1"))?;
        self.with_live(call, None, |store, instance| {
            let guest = indices.load(&mut *store, instance)?;
            f(&guest, store)
        })
    }
}
