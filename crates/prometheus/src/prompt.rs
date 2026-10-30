//! Terminal interaction: no-echo secret entry, typed confirmations and the
//! TTY checks that keep secrets out of pipes. Prompts go to the terminal
//! or stderr, never to stdout.

use std::io::{self, BufRead as _, IsTerminal as _, Read as _, Write as _};

use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;

/// Whether stdin is an interactive terminal.
#[must_use]
pub fn stdin_is_tty() -> bool {
    io::stdin().is_terminal()
}

/// Whether stdout is an interactive terminal.
#[must_use]
pub fn stdout_is_tty() -> bool {
    io::stdout().is_terminal()
}

/// Reads a secret value: from a no-echo prompt on a terminal, else all of
/// stdin with one trailing line break removed (`echo $TOKEN | ...`).
///
/// # Errors
///
/// `secret.empty` for an empty value, `output.io` when reading fails.
pub fn read_secret(name: &str) -> Result<SecretString, Error> {
    let value = if stdin_is_tty() {
        rpassword::prompt_password(format!("value for {name} (input hidden): "))
            .map_err(|e| read_error(&e))?
    } else {
        let mut text = String::new();
        io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| read_error(&e))?;
        let trimmed = text
            .strip_suffix("\r\n")
            .or_else(|| text.strip_suffix('\n'))
            .unwrap_or(&text);
        trimmed.to_owned()
    };
    if value.is_empty() {
        return Err(Error::new(
            &codes::SECRET_EMPTY,
            format!("no value was given for {name}"),
        ));
    }
    Ok(SecretString::new(value))
}

/// Asks the user to type `expected` to confirm `what`. `Ok(None)` when no
/// terminal can answer (the caller exits 4); `Ok(Some(confirmed))` else.
///
/// # Errors
///
/// `output.io` when reading the answer fails.
pub fn confirm_typed(what: &str, expected: &str) -> Result<Option<bool>, Error> {
    if !stdin_is_tty() {
        return Ok(None);
    }
    eprint!("{what}\ntype '{expected}' to confirm: ");
    let _ = io::stderr().flush();
    let mut answer = String::new();
    io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|e| read_error(&e))?;
    Ok(Some(answer.trim() == expected))
}

/// Exit status when a confirmation is required but no terminal can give
/// it (design: shared exit codes).
pub const NEEDS_CONFIRMATION: u8 = 4;

fn read_error(e: &io::Error) -> Error {
    Error::new(&codes::OUTPUT_IO, format!("cannot read from stdin: {e}"))
}
