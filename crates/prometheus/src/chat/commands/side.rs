//! `/btw <question>` (spec: agent-turn-loop, Side questions with /btw):
//! answered outside the transcript, even while a turn runs. The snapshot
//! is taken when the command is issued; the answer is posted when it
//! arrives (a side panel line on the CLI, TUI and desktop, a separate
//! message on messaging platforms), labelled as a side answer.

use std::sync::Arc;

use pmt_core::error::{Error, codes};
use pmt_store::Visibility;

use super::Ctx;

/// The label of every side answer.
const LABEL: &str = "side answer";

pub(super) fn btw(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let question = args.trim();
    if question.is_empty() {
        return Err(Error::new(&codes::CLI_USAGE, "usage: /btw <question>"));
    }
    let live = ctx.live;
    let rows = ctx.factory.store.messages(live.id, Visibility::Active)?;
    let transcript = pmt_agent::turn::repair::wire(&rows);
    let asker = ctx.factory.models.side(&live.model, live.id);
    let post = Arc::clone(&ctx.post);
    let pending = question.to_owned();
    pmt_core::spawn::spawn(async move {
        let text = match asker.ask(transcript, &pending).await {
            Ok(answer) => format!("{LABEL}: {}", answer.trim()),
            Err(why) => format!("{LABEL} failed: {why}"),
        };
        post(text);
    });
    Ok(format!("{LABEL} pending: {question}"))
}
