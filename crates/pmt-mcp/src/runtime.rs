//! The runtime every MCP connection lives on.
//!
//! An MCP connection is a set of tasks (the transport, the request
//! router, a child process's pipes). Sessions, the daemon and one-shot
//! commands each run their own tokio runtime, and a profile's servers are
//! shared between them, so the connections run on one process-wide
//! runtime that is never shut down; callers await its tasks from any
//! runtime.

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::{Builder, Handle, Runtime};

/// Worker threads of the MCP runtime (connections are I/O bound).
const WORKERS: usize = 2;

fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .worker_threads(WORKERS)
            .thread_name("pmt-mcp")
            .enable_all()
            .build()
            .unwrap_or_else(|err| panic!("the MCP runtime could not start: {err}"))
    })
}

/// The MCP runtime's handle.
#[must_use]
pub fn handle() -> Handle {
    runtime().handle().clone()
}

/// Runs `future` on the MCP runtime and waits for it from any runtime (a
/// panic in it resumes here).
pub async fn run<F>(future: F) -> F::Output
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    match pmt_core::spawn::spawn_on(&handle(), future).await {
        Ok(output) => output,
        Err(err) => std::panic::resume_unwind(err.into_panic()),
    }
}
