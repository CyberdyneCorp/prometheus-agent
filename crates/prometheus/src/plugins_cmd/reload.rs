//! `prometheus plugins reload [<id>]` (spec: plugin-system, Hot reload
//! respects the prompt-cache invariant). With a running daemon the reload
//! happens there, for the sessions it serves; otherwise no session runs
//! and the next one loads the plugins anew.

use pmt_core::error::{Error, codes};
use pmt_rpc::control::ControlRequest;

use crate::context::Context;
use crate::output::print_raw;
use crate::plugins_host;

/// Runs the reload and prints what is active now and what is deferred.
pub(super) fn reload(ctx: &Context<'_>, id: Option<&str>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let report = if let Some(record) = crate::daemon_client::running(scope.home())? {
        in_daemon(&record, scope.name().as_str(), id)?
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| scope.dir().to_path_buf());
        let loaded = plugins_host::reload(&scope, &cwd).current();
        plugins_host::reload::check_target(&loaded, id)?;
        plugins_host::reload::report(&loaded, id, false)
    };
    print_raw(&format!("{report}\n"))
}

/// Asks the running daemon to reload `profile`'s plugins.
fn in_daemon(
    record: &pmt_rpc::discovery::DaemonRecord,
    profile: &str,
    id: Option<&str>,
) -> Result<String, Error> {
    let request = ControlRequest::PluginsReload {
        profile: profile.to_owned(),
        plugin: id.map(str::to_owned),
    };
    let control = record.endpoint.control.clone();
    let response =
        crate::model_cmd::block_on(
            async move { pmt_rpc::control::call(&control, &request).await },
        )??;
    if let Some(err) = response.error {
        let code = pmt_core::error::ERROR_CODES
            .iter()
            .find(|c| c.code == err.code)
            .copied()
            .unwrap_or(&codes::PLUGINS_LOAD);
        return Err(Error::new(code, err.message));
    }
    Ok(response.reloaded.unwrap_or_default())
}
