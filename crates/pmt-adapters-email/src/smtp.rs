//! Sending one message over SMTP (RFC 5321 subset): EHLO, STARTTLS when
//! asked, AUTH PLAIN, MAIL/RCPT/DATA with dot-stuffing, QUIT. Reply codes
//! map onto the gateway's send errors: a `4.7.x` deferral is throttling,
//! other `4xx` are temporary, `5xx` refusals are permanent.

use base64::Engine as _;
use pmt_config::platforms::MailSecurity;
use pmt_gateway::messaging::SendError;

use crate::wire::{TlsPolicy, Wire, WireError};

/// Where and as whom to send.
pub(crate) struct Relay<'a> {
    pub(crate) host: &'a str,
    pub(crate) port: u16,
    pub(crate) security: MailSecurity,
    pub(crate) tls: &'a TlsPolicy,
    pub(crate) user: &'a str,
    pub(crate) password: &'a str,
}

fn network(err: &WireError) -> SendError {
    SendError::Network(err.to_string())
}

/// The send error of an SMTP reply.
pub(crate) fn refusal(code: u16, text: &str) -> SendError {
    let enhanced = text.split_whitespace().next().unwrap_or_default();
    match code {
        421 | 450 | 451 | 452 if enhanced.starts_with("4.7.") => {
            SendError::RateLimited { retry_after: None }
        }
        400..=499 => SendError::Server(format!("{code} {text}")),
        _ => SendError::Permanent(format!("{code} {text}")),
    }
}

struct Session {
    wire: Wire,
}

impl Session {
    /// Reads a (possibly multi-line) reply: its code and last text.
    async fn reply(&mut self) -> Result<(u16, String), SendError> {
        loop {
            let line = self.wire.line().await.map_err(|e| network(&e))?;
            let code = line
                .get(..3)
                .and_then(|c| c.parse().ok())
                .ok_or_else(|| SendError::Server(format!("not an SMTP reply: {line}")))?;
            if line.as_bytes().get(3) != Some(&b'-') {
                return Ok((code, line.get(4..).unwrap_or_default().to_owned()));
            }
        }
    }

    /// Sends `line` and expects a reply in `want`.
    async fn expect(&mut self, line: Option<&str>, want: u16) -> Result<(), SendError> {
        if let Some(line) = line {
            self.wire
                .write(format!("{line}\r\n").as_bytes())
                .await
                .map_err(|e| network(&e))?;
        }
        let (code, text) = self.reply().await?;
        if code == want {
            Ok(())
        } else {
            Err(refusal(code, &text))
        }
    }
}

/// Dot-stuffs a message body and ends it.
fn data(message: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(message).replace("\r\n", "\n");
    let mut out = String::new();
    for line in text.split('\n') {
        if line.starts_with('.') {
            out.push('.');
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str(".\r\n");
    out.into_bytes()
}

/// Sends `message` from `from` to `to`.
pub(crate) async fn send(
    relay: &Relay<'_>,
    from: &str,
    to: &str,
    message: &[u8],
) -> Result<(), SendError> {
    let wire = Wire::connect(relay.host, relay.port, relay.security, relay.tls)
        .await
        .map_err(|e| network(&e))?;
    let mut session = Session { wire };
    session.expect(None, 220).await?;
    session.expect(Some("EHLO prometheus"), 250).await?;
    if relay.security == MailSecurity::Starttls {
        session.expect(Some("STARTTLS"), 220).await?;
        session.wire = session
            .wire
            .upgrade(relay.tls)
            .await
            .map_err(|e| network(&e))?;
        session.expect(Some("EHLO prometheus"), 250).await?;
    }
    let auth = base64::engine::general_purpose::STANDARD
        .encode(format!("\0{}\0{}", relay.user, relay.password));
    session
        .expect(Some(&format!("AUTH PLAIN {auth}")), 235)
        .await?;
    session
        .expect(Some(&format!("MAIL FROM:<{from}>")), 250)
        .await?;
    session
        .expect(Some(&format!("RCPT TO:<{to}>")), 250)
        .await?;
    session.expect(Some("DATA"), 354).await?;
    session
        .wire
        .write(&data(message))
        .await
        .map_err(|e| SendError::Ambiguous(e.to_string()))?;
    session.expect(None, 250).await?;
    let _ = session.expect(Some("QUIT"), 221).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_map_onto_send_errors() {
        assert_eq!(
            refusal(451, "4.7.1 Try again later, rate limited"),
            SendError::RateLimited { retry_after: None }
        );
        assert!(matches!(
            refusal(451, "4.3.0 Mail server temporarily rejected"),
            SendError::Server(_)
        ));
        assert!(matches!(
            refusal(550, "5.1.1 No such user"),
            SendError::Permanent(_)
        ));
    }

    #[test]
    fn leading_dots_are_stuffed() {
        assert_eq!(data(b"a\r\n.b\n"), b"a\r\n..b\r\n\r\n.\r\n");
    }
}
