//! Quality gates run at a goal's turn boundary (spec: session-automation,
//! Quality gates): each command runs on the session's own terminal — its
//! backend, sandbox and working directory — with `goals.gate_timeout_secs`.
//! A gate that cannot run counts as failed (exit `-1`), so a broken gate
//! never lets a goal through.

use pmt_automation::goal::GateRun;
use pmt_automation::state::Gate;
use pmt_core::id::SessionId;
use pmt_tools::environments::Environments;
use pmt_tools::terminal::TerminalArgs;

/// Runs every gate of the goal, in order (each on every boundary: a failed
/// gate's result is never replayed).
pub(super) async fn run(session: SessionId, gates: &[Gate], timeout_secs: u64) -> Vec<GateRun> {
    let terminal = Environments::process()
        .session(&session.to_string())
        .and_then(|env| env.terminal.clone());
    let mut runs = Vec::with_capacity(gates.len());
    for gate in gates {
        let Some(terminal) = &terminal else {
            runs.push(GateRun {
                exit: -1,
                output: "the session has no terminal to run quality gates on".to_owned(),
            });
            continue;
        };
        let args = TerminalArgs {
            #[allow(clippy::cast_precision_loss)] // whole seconds, far below 2^52
            timeout: Some(timeout_secs as f64),
            ..TerminalArgs::new(gate.command.clone())
        };
        let result = terminal.run(args).await;
        let mut output = result.output;
        if let Some(error) = result.error {
            output.push('\n');
            output.push_str(&error);
        }
        runs.push(GateRun {
            exit: result.exit_code.unwrap_or(-1),
            output,
        });
    }
    runs
}
