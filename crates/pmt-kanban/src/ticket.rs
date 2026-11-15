//! Run tickets (spec: kanban, Worker spawn by run ticket): the worker's
//! whole context reaches it as one JSON line on stdin — board and run,
//! plus the single-use secret whose hash the run row holds — instead of
//! environment variables a child could inherit or a process listing
//! could show.

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

/// The ticket format version.
pub const VERSION: u32 = 1;

/// One run ticket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ticket {
    /// [`VERSION`].
    pub v: u32,
    /// The board slug.
    pub board: String,
    /// The run id.
    pub run: String,
    /// The single-use secret.
    pub secret: String,
}

impl Ticket {
    /// The JSON line written to the worker's stdin.
    #[must_use]
    pub fn line(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Parses what the worker read from stdin, checking it is for `run`.
    ///
    /// # Errors
    ///
    /// `kanban.ticket_refused`.
    pub fn parse(text: &str, run: &str) -> Result<Self, Error> {
        let ticket = Self::read(text)?;
        if ticket.run != run {
            return Err(refused("run ticket does not match an open run"));
        }
        Ok(ticket)
    }

    /// Parses a ticket line (the first line of `text`).
    ///
    /// # Errors
    ///
    /// `kanban.ticket_refused`.
    pub fn read(text: &str) -> Result<Self, Error> {
        let line = text.lines().next().unwrap_or("").trim();
        if line.is_empty() {
            return Err(refused("run ticket missing on stdin"));
        }
        let ticket: Self =
            serde_json::from_str(line).map_err(|_| refused("run ticket malformed"))?;
        if ticket.v != VERSION {
            return Err(refused("run ticket version not supported"));
        }
        Ok(ticket)
    }
}

fn refused(message: &str) -> Error {
    Error::new(&codes::KANBAN_TICKET_REFUSED, message.to_owned())
}
