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
            let strings = ctx.strings();
            let server = match (status.server.source, status.server.port) {
                (Some(source), Some(port)) => {
                    pmt_i18n::t!(strings, "slash.local.running", source = source, port = port)
                }
                _ => pmt_i18n::t!(strings, "slash.local.stopped"),
            };
            let resident = if status.resident.is_empty() {
                pmt_i18n::t!(strings, "slash.local.none")
            } else {
                status.resident.join(", ")
            };
            Ok(pmt_i18n::t!(
                strings,
                "slash.local.status",
                pinned = status.engine.pinned,
                backend = status.engine.backend,
                server = server,
                resident = resident
            ))
        }
        _ => Ok(USAGE.to_owned()),
    }
}
