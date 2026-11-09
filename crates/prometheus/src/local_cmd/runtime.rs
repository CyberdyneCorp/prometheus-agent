//! The local-models runtime of this process: what a `llamacpp` route
//! without a `base_url` resolves through (spec: local-models, Managed
//! server lifecycle), and the hidden `local supervise` helper that
//! supervises a managed server for the process that started it.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_core::error::Error;
use pmt_local::server::{self, HelperCommand, Runtime, Stop};
use pmt_providers::local_runtime::LocalRuntime;
use pmt_providers::resolve::BoxFuture;

use crate::context::Context;

/// The runtime of `home`: detects, reuses or starts the llama-server.
#[must_use]
pub fn for_home(home: &Path) -> Arc<dyn LocalRuntime> {
    Arc::new(HostRuntime(Runtime::new(home, Some(helper(home)))))
}

struct HostRuntime(Runtime);

impl LocalRuntime for HostRuntime {
    fn endpoint(&self) -> BoxFuture<'_, Result<String, Error>> {
        Box::pin(async move { self.0.endpoint().await.map(|e| e.base_url) })
    }
}

/// `prometheus --home <home> local supervise`, run by this binary.
pub(crate) fn helper(home: &Path) -> HelperCommand {
    let home = home.to_path_buf();
    Arc::new(move || {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("prometheus"));
        let mut command = std::process::Command::new(exe);
        command
            .arg("--home")
            .arg(&home)
            .args(["local", "supervise"])
            .stderr(std::process::Stdio::null());
        command
    })
}

/// `local supervise`: supervises the managed server until stdin closes
/// (the client process exited), printing `ready <port>` once it answers.
///
/// # Errors
///
/// As [`server::serve`].
pub(super) fn supervise(home: &Path) -> Result<u8, Error> {
    let stop = Stop::default();
    let watcher = stop.clone();
    std::thread::spawn(move || {
        let mut sink = [0; 64];
        let mut stdin = std::io::stdin();
        while matches!(stdin.read(&mut sink), Ok(n) if n > 0) {}
        watcher.raise();
    });
    let mut announced = false;
    server::serve(home, &stop, &mut |record| {
        if !announced {
            announced = true;
            let mut out = std::io::stdout();
            // The client may have stopped reading; nothing to do then.
            let _ = writeln!(out, "ready {}", record.port);
            let _ = out.flush();
        }
    })?;
    Ok(0)
}

/// Asks a running daemon to supervise the server; `None` when no daemon
/// serves this home.
///
/// # Errors
///
/// The daemon's failure.
pub(super) fn start_in_daemon(ctx: &Context<'_>) -> Result<Option<String>, Error> {
    let home = ctx.home()?;
    let Some(record) = crate::daemon_client::running(home.root())? else {
        return Ok(None);
    };
    let profile = home.select_profile(ctx.profile)?;
    crate::model_cmd::block_on(async move {
        let hello = crate::daemon_client::hello(pmt_rpc::contracts::ClientKind::Cli, false);
        let (client, _inbox) =
            crate::daemon_client::connect_cli(&record, profile.as_str(), &hello).await?;
        let answer = client.call("local.start", serde_json::json!({})).await;
        client.close();
        let reply: pmt_rpc::contracts::LocalReply =
            serde_json::from_value(answer.map_err(|e| pmt_rpc::client::error_of(&e))?)
                .map_err(|e| Error::new(&pmt_core::error::codes::RPC_CONNECTION, e.to_string()))?;
        Ok(Some(format!(
            "{} (supervised by the daemon)",
            reply.message
        )))
    })?
}
