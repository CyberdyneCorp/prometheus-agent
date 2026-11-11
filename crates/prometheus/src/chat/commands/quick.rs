//! Quick commands (spec: cli, Quick commands): `[quick_commands.<name>]`
//! entries of type `alias` re-dispatch their target with the user's
//! arguments appended; `exec` entries run their command on the session's
//! sandboxed terminal (an environment without secrets, output redacted)
//! and are killed after `timeout_secs`.

use std::collections::BTreeMap;

use pmt_config::schema::{QuickCommand, QuickCommandKind};
use pmt_tools::environments::Environments;
use pmt_tools::terminal::{Status, TerminalArgs};

use super::Ctx;
use crate::slash::{Declaration, Registry, Source};

/// Alias chains deeper than this stop (config validation refuses cycles;
/// this bounds chains through other commands).
pub(crate) const MAX_DEPTH: u8 = 8;

/// Registers the profile's quick commands.
pub(super) fn register(
    registry: &mut Registry,
    commands: &BTreeMap<String, QuickCommand>,
    strings: &pmt_i18n::Strings,
) {
    for (name, command) in commands {
        let description = match command.kind {
            QuickCommandKind::Alias => {
                let target = command.target.as_deref().unwrap_or("");
                pmt_i18n::t!(strings, "slash.quick.alias", target = target)
            }
            QuickCommandKind::Exec => {
                let line = command.command.as_deref().unwrap_or("");
                pmt_i18n::t!(strings, "slash.quick.runs", command = line)
            }
        };
        let declaration = Declaration {
            name: name.clone(),
            description,
            ..Declaration::default()
        };
        if let Err(err) = registry.register(&declaration, Source::Quick) {
            tracing::warn!(error = %err, "quick command not loaded");
        }
    }
}

/// The slash line an `alias` quick command runs.
#[must_use]
pub(crate) fn alias_line(command: &QuickCommand, args: &str) -> String {
    let target = command.target.as_deref().unwrap_or_default().trim();
    let target = target.strip_prefix('/').unwrap_or(target);
    if args.is_empty() {
        format!("/{target}")
    } else {
        format!("/{target} {args}")
    }
}

/// Runs `exec` quick command `name` on the session's terminal: its
/// redacted output, then a note when it timed out or failed.
pub(crate) async fn exec(ctx: &Ctx<'_>, name: &str, command: &QuickCommand, args: &str) -> String {
    let strings = ctx.strings();
    let Some(terminal) = Environments::process()
        .session(&ctx.live.id.to_string())
        .and_then(|env| env.terminal.clone())
    else {
        return pmt_i18n::t!(strings, "slash.quick.no_terminal", name = name);
    };
    let line = command.command.as_deref().unwrap_or_default();
    let line = if args.is_empty() {
        line.to_owned()
    } else {
        format!("{line} {args}")
    };
    let mut run = TerminalArgs::new(line);
    #[allow(clippy::cast_precision_loss)] // whole seconds, far below 2^52
    let seconds = command.timeout_secs as f64;
    run.timeout = Some(seconds);
    let result = terminal.run(run).await;
    let mut text = ctx.factory.redactor().redact_all(result.output.trim_end());
    let note = match result.status {
        Status::Timeout => Some(pmt_i18n::t!(
            strings,
            "slash.quick.timed_out",
            name = name,
            secs = command.timeout_secs
        )),
        Status::Error => Some(match result.error {
            Some(why) => pmt_i18n::t!(strings, "slash.quick.failed_why", name = name, why = why),
            None => pmt_i18n::t!(strings, "slash.quick.failed", name = name),
        }),
        _ => None,
    };
    if let Some(note) = note {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&note);
    }
    text
}
