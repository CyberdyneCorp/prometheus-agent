//! `/local use <model> [--global]` and `/local status` (spec:
//! local-models, Local models command): `use` is `/model llamacpp/<model>
//! --session` (or `--global`, which also saves `model.provider` and
//! `model.default`) after checking the model is in the library.

use pmt_core::error::Error;

use super::{Ctx, session};

const USAGE: &str = "usage: /local use <model> [--global] | /local status";

/// Runs `/local`.
pub(super) async fn local(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let mut words: Vec<&str> = args.split_whitespace().collect();
    let global = words.contains(&"--global");
    words.retain(|w| *w != "--global");
    let home = ctx.scope.home();
    match words.as_slice() {
        ["use", model] => {
            let library = pmt_local::library::Library::open(home);
            let found = library.model(model, &pmt_local::download::Jobs::open(home))?;
            let scope = if global { "--global" } else { "--session" };
            session::model(ctx, &format!("llamacpp/{} {scope}", found.id)).await
        }
        ["status"] => {
            let status = pmt_local::status::status(home, pmt_local::hardware::probe()).await?;
            let server = match (status.server.source, status.server.port) {
                (Some(source), Some(port)) => format!("running ({source}) on 127.0.0.1:{port}"),
                _ => "stopped".to_owned(),
            };
            Ok(format!(
                "llama.cpp {} ({}); server {server}; resident: {}",
                status.engine.pinned,
                status.engine.backend,
                if status.resident.is_empty() {
                    "none".to_owned()
                } else {
                    status.resident.join(", ")
                }
            ))
        }
        _ => Ok(USAGE.to_owned()),
    }
}
