//! `/moa <prompt>` (spec: mixture-of-agents, One-shot /moa command): one
//! user turn with the whole argument as its literal prompt on the
//! default Mixture of Agents preset, as a `/model --once` switch, so the
//! session's previous route is back when that turn ends, however it
//! ends. The argument is never read as a preset name.

use pmt_agent::turn::TurnInput;
use pmt_core::error::Error;

use super::{Ctx, Reply, session};

/// What a bare `/moa` prints.
pub(crate) const USAGE: &str = "usage: /moa <prompt> — runs one turn on the default Mixture of Agents preset (moa.default_preset), then returns to the session's model";

/// Switches the next turn to the default preset and sends `args`.
pub(super) async fn moa(ctx: &Ctx<'_>, args: &str) -> Result<Reply, Error> {
    let prompt = args.trim();
    if prompt.is_empty() {
        return Ok(Reply::Text(USAGE.to_owned()));
    }
    let preset = &ctx.scope.config().moa.default_preset;
    session::model(ctx, &format!("moa/{preset} --once")).await?;
    Ok(Reply::Send(TurnInput::text(prompt)))
}
