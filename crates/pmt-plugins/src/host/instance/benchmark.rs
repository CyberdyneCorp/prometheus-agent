//! Calls into the `bench` world (spec: replay-and-evaluation, Benchmark
//! adapter protocol).

use std::sync::Arc;

use pmt_core::error::Error;

use super::{CallInfo, Instance};
use crate::host::ports::BenchPort;

impl Instance {
    /// `plugin.call(method, params)` with `port` serving the call's
    /// `files` and `requests` imports: the plugin's own JSON-RPC error is
    /// returned as `Ok(Err((code, message)))`, a trap or timeout as `Err`.
    ///
    /// # Errors
    ///
    /// `plugins.load` when the component does not export the bench world;
    /// `plugins.call` when the call traps or times out.
    pub fn bench_call(
        &self,
        call: &CallInfo,
        port: Arc<dyn BenchPort>,
        method: &str,
        params: &str,
    ) -> Result<Result<String, (i64, String)>, Error> {
        let indices = self.bench.as_ref().ok_or_else(|| self.missing("bench"))?;
        self.with_live(call, None, |store, instance| {
            store.data_mut().bench = Some(port);
            let reply = indices
                .load(&mut *store, instance)
                .and_then(|plugin| plugin.call_call(&mut *store, method, params));
            store.data_mut().bench = None;
            reply.map(|r| r.map_err(|e| (e.code, e.message)))
        })
    }
}
