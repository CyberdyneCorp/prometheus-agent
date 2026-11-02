//! The IMAP commands the adapter needs (RFC 9051 subset): log in
//! (STARTTLS first when asked), select the inbox, search unseen messages,
//! fetch one without marking it, mark it seen. UIDs are used throughout so
//! a reconnect names the same messages.

use std::path::Path;

use pmt_config::platforms::MailSecurity;

use crate::wire::{Wire, WireError};

/// Why an IMAP step failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImapError {
    /// The connection failed (retry later).
    Wire(WireError),
    /// The server refused a command (`NO`/`BAD`).
    Refused(String),
}

impl From<WireError> for ImapError {
    fn from(err: WireError) -> Self {
        Self::Wire(err)
    }
}

impl std::fmt::Display for ImapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wire(err) => write!(f, "{err}"),
            Self::Refused(why) => write!(f, "refused: {why}"),
        }
    }
}

/// One untagged response line with its literal, when it carried one.
#[derive(Debug)]
struct Untagged {
    line: String,
    literal: Option<Vec<u8>>,
}

/// An IMAP string, quoted.
fn quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The literal length a line announces (`... {123}`).
fn literal_len(line: &str) -> Option<usize> {
    let open = line.strip_suffix('}')?.rfind('{')?;
    line[open + 1..line.len() - 1].parse().ok()
}

/// A logged-in session with the inbox selected.
pub(crate) struct Imap {
    wire: Wire,
    tag: u32,
}

/// Where and as whom to log in.
pub(crate) struct Login<'a> {
    pub(crate) host: &'a str,
    pub(crate) port: u16,
    pub(crate) security: MailSecurity,
    pub(crate) extra_ca: Option<&'a Path>,
    pub(crate) user: &'a str,
    pub(crate) password: &'a str,
}

impl Imap {
    /// Connects, logs in and selects `INBOX`.
    pub(crate) async fn login(login: &Login<'_>) -> Result<Self, ImapError> {
        let wire = Wire::connect(login.host, login.port, login.security, login.extra_ca).await?;
        let mut session = Self { wire, tag: 0 };
        let greeting = session.wire.line().await?;
        if !greeting.starts_with("* OK") && !greeting.starts_with("* PREAUTH") {
            return Err(ImapError::Refused(greeting));
        }
        if login.security == MailSecurity::Starttls {
            session.command("STARTTLS").await?;
            session.wire = session.wire.upgrade(login.extra_ca).await?;
        }
        let command = format!("LOGIN {} {}", quote(login.user), quote(login.password));
        session.command(&command).await?;
        session.command("SELECT INBOX").await?;
        Ok(session)
    }

    async fn command(&mut self, command: &str) -> Result<Vec<Untagged>, ImapError> {
        self.tag += 1;
        let tag = format!("a{}", self.tag);
        self.wire
            .write(format!("{tag} {command}\r\n").as_bytes())
            .await?;
        let mut untagged = Vec::new();
        loop {
            let mut line = self.wire.line().await?;
            if let Some(status) = line.strip_prefix(&format!("{tag} ")) {
                return if status.starts_with("OK") {
                    Ok(untagged)
                } else {
                    Err(ImapError::Refused(status.to_owned()))
                };
            }
            let mut literal = None;
            if let Some(len) = literal_len(&line) {
                literal = Some(self.wire.exact(len).await?);
                // The rest of the response follows the literal.
                line.push_str(&self.wire.line().await?);
            }
            untagged.push(Untagged { line, literal });
        }
    }

    /// UIDs of the unseen messages.
    pub(crate) async fn unseen(&mut self) -> Result<Vec<u32>, ImapError> {
        let found = self.command("UID SEARCH UNSEEN").await?;
        Ok(found
            .iter()
            .filter_map(|u| u.line.strip_prefix("* SEARCH"))
            .flat_map(|ids| ids.split_whitespace().filter_map(|id| id.parse().ok()))
            .collect())
    }

    /// The raw message `uid`, left unseen.
    pub(crate) async fn fetch(&mut self, uid: u32) -> Result<Option<Vec<u8>>, ImapError> {
        let found = self
            .command(&format!("UID FETCH {uid} (BODY.PEEK[])"))
            .await?;
        Ok(found.into_iter().find_map(|u| u.literal))
    }

    /// Marks `uid` seen.
    pub(crate) async fn mark_seen(&mut self, uid: u32) -> Result<(), ImapError> {
        self.command(&format!("UID STORE {uid} +FLAGS (\\Seen)"))
            .await
            .map(drop)
    }

    /// Logs out (best effort).
    pub(crate) async fn logout(mut self) {
        let _ = self.command("LOGOUT").await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_are_quoted_and_literals_found() {
        assert_eq!(quote(r#"pa"ss\word"#), r#""pa\"ss\\word""#);
        assert_eq!(literal_len("* 1 FETCH (UID 7 BODY[] {342}"), Some(342));
        assert_eq!(literal_len("* SEARCH 1 2"), None);
    }
}
