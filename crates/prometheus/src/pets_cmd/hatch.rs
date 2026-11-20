//! `pets hatch <description>`: `/hatch` at the shell. The backend is
//! chosen (and refused, exit 78, spending nothing) before anything is
//! generated; each paid step asks on the terminal.

use std::io::{BufRead as _, Write as _};

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_pets::hatch::{Hatch, Step};

use crate::context::Context;

pub(super) fn run(
    _ctx: &Context<'_>,
    scope: &ProfileScope,
    description: &str,
) -> Result<u8, Error> {
    let backend = crate::pets_host::backend::select(scope)?;
    if !crate::prompt::stdin_is_tty() {
        return Err(Error::new(
            &codes::CLI_USAGE,
            "pet generation asks before each paid step: run it on a terminal",
        ));
    }
    let drafts = scope.dir().join("cache/pets/hatch");
    let (mut hatch, mut step) =
        Hatch::start(description, backend, crate::pets_host::pets(scope), drafts);
    let runtime = super::runtime()?;
    loop {
        match step {
            Step::Ask(text) => {
                eprint!("{text} ");
                let _ = std::io::stderr().flush();
                let mut answer = String::new();
                std::io::stdin()
                    .lock()
                    .read_line(&mut answer)
                    .map_err(|e| crate::prompt::read_error(&e))?;
                step = runtime.block_on(hatch.answer(&answer));
            }
            Step::Done { text, failed, .. } => {
                crate::output::print(&text)?;
                return Ok(u8::from(failed));
            }
        }
    }
}
