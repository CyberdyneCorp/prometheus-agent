//! What a session's skills see of it (spec: skills, Platform, environment
//! and conditional gates; Load-time template substitution and inline
//! shell; Skill secrets and settings): the surface and its frozen tool
//! selection, and the session's sandbox for inline snippets.

use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use pmt_skills::gates::SessionView;
use pmt_skills::inline::{BoxFuture, InlineShell, SandboxShell};
use pmt_tools::Selection;

/// The CLI surface's view of `selection` (the tools the session offers).
#[must_use]
pub fn view(selection: &Selection) -> SessionView {
    SessionView {
        platform: Some("cli".to_owned()),
        tools: selection.tools.iter().cloned().collect(),
        toolsets: selection.toolsets.iter().cloned().collect(),
    }
}

/// The session's inline-shell runner, bound once its sandbox and command
/// gate exist (the skills session is built before them, because the
/// sandbox grants reads of the skills' directories).
#[derive(Default)]
pub struct DeferredShell {
    shell: OnceLock<SandboxShell>,
}

impl DeferredShell {
    /// Runs snippets with `shell` from now on.
    pub fn bind(&self, shell: SandboxShell) {
        let _ = self.shell.set(shell);
    }
}

impl InlineShell for DeferredShell {
    fn run<'a>(
        &'a self,
        command: &'a str,
        cwd: &'a Path,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<String, String>> {
        match self.shell.get() {
            Some(shell) => shell.run(command, cwd, timeout),
            None => Box::pin(async { Err("no execution sandbox in this session".to_owned()) }),
        }
    }
}

/// Binds `deferred` to the session's commands and gate, when the session
/// could set up a sandbox and shell.
pub fn bind_shell(
    deferred: &Arc<DeferredShell>,
    commands: Option<&super::env::Commands>,
    gate: &Arc<pmt_tools::approval::CommandGate>,
) {
    if let Some(commands) = commands {
        deferred.bind(SandboxShell::new(
            Arc::clone(&commands.sandbox),
            commands.env.clone(),
            commands.shell.clone(),
            Some(Arc::clone(gate)),
        ));
    }
}
