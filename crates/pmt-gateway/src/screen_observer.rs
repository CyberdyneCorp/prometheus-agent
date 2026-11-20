//! What an observer may send on the bot screen's display stream (spec:
//! tui, observers are read-only; execution-environments, Bot screen): the
//! RFB handshake (RFC 6143 §7.1-7.3, `ClientInit` forced to shared) and the
//! messages that only shape what the viewer receives (`SetPixelFormat`,
//! `SetEncodings`, `FramebufferUpdateRequest`). Key and pointer events and
//! clipboard text would drive the bot's desktop, so they are dropped;
//! anything else is refused, since its length is unknown.
//!
//! WebSocket frames split RFB messages anywhere, so the filter buffers a
//! partial message until it is complete.

/// The largest `ClientCutText` an observer's stream may carry (it is
/// dropped, but must be read past).
const MAX_CUT_TEXT: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Version,
    Security,
    Init,
    Messages,
}

/// The observer's side of one display stream.
#[derive(Debug)]
pub(crate) struct ObserverFilter {
    stage: Stage,
    pending: Vec<u8>,
}

/// One complete unit at the head of the buffer: forward it, drop it, or
/// refuse the stream.
enum Unit {
    Forward(usize),
    Drop(usize),
    Incomplete,
}

impl ObserverFilter {
    pub(crate) fn new() -> Self {
        Self {
            stage: Stage::Version,
            pending: Vec::new(),
        }
    }

    /// The bytes of `data` (with what earlier frames left incomplete) the
    /// screen may receive.
    ///
    /// # Errors
    ///
    /// Why the stream is refused: a security type other than None or a
    /// message an observer may not send.
    pub(crate) fn feed(&mut self, data: &[u8]) -> Result<Vec<u8>, &'static str> {
        self.pending.extend_from_slice(data);
        let mut out = Vec::new();
        loop {
            match self.next()? {
                Unit::Incomplete => return Ok(out),
                Unit::Forward(n) => out.extend(self.pending.drain(..n)),
                Unit::Drop(n) => drop(self.pending.drain(..n)),
            }
        }
    }

    fn next(&mut self) -> Result<Unit, &'static str> {
        let buf = &mut self.pending;
        let Some(&first) = buf.first() else {
            return Ok(Unit::Incomplete);
        };
        match self.stage {
            Stage::Version => {
                if buf.len() < 12 {
                    return Ok(Unit::Incomplete);
                }
                // RFB 3.3 has the server pick the security type.
                self.stage = if &buf[..12] == b"RFB 003.003\n" {
                    Stage::Init
                } else {
                    Stage::Security
                };
                Ok(Unit::Forward(12))
            }
            Stage::Security => {
                if first != 1 {
                    return Err("an observer's viewer must use security type None");
                }
                self.stage = Stage::Init;
                Ok(Unit::Forward(1))
            }
            Stage::Init => {
                buf[0] = 1; // shared: watching never disconnects others
                self.stage = Stage::Messages;
                Ok(Unit::Forward(1))
            }
            Stage::Messages => message(buf),
        }
    }
}

/// The next client-to-server message (RFC 6143 §7.5).
fn message(buf: &[u8]) -> Result<Unit, &'static str> {
    let fixed = |len: usize, keep: bool| {
        Ok(match (buf.len() >= len, keep) {
            (false, _) => Unit::Incomplete,
            (true, true) => Unit::Forward(len),
            (true, false) => Unit::Drop(len),
        })
    };
    match buf[0] {
        0 => fixed(20, true), // SetPixelFormat
        2 => {
            // SetEncodings: type, padding, u16 count, count x s32.
            let Some(count) = buf.get(2..4) else {
                return Ok(Unit::Incomplete);
            };
            fixed(
                4 + 4 * usize::from(u16::from_be_bytes([count[0], count[1]])),
                true,
            )
        }
        3 => fixed(10, true), // FramebufferUpdateRequest
        4 => fixed(8, false), // KeyEvent
        5 => fixed(6, false), // PointerEvent
        6 => {
            // ClientCutText: type, 3 padding, u32 length, text.
            let Some(length) = buf.get(4..8) else {
                return Ok(Unit::Incomplete);
            };
            let length = u32::from_be_bytes([length[0], length[1], length[2], length[3]]);
            let length = usize::try_from(length).unwrap_or(usize::MAX);
            if length > MAX_CUT_TEXT {
                return Err("clipboard text too long");
            }
            fixed(8 + length, false)
        }
        _ => Err("an observer may only watch the bot screen"),
    }
}

#[cfg(test)]
mod tests {
    use super::ObserverFilter;

    const HANDSHAKE: &[u8] = b"RFB 003.008\n\x01\x00";
    const UPDATE: [u8; 10] = [3, 0, 0, 0, 0, 0, 0x05, 0xa0, 0x03, 0x84];
    const POINTER: [u8; 6] = [5, 1, 0, 10, 0, 20];
    const KEY: [u8; 8] = [4, 1, 0, 0, 0, 0, 0, 0x61];

    #[test]
    fn an_observer_watches_but_its_input_never_reaches_the_screen() {
        let mut filter = ObserverFilter::new();
        let mut sent = HANDSHAKE.to_vec();
        sent.extend([2, 0, 0, 1, 0, 0, 0, 0]); // SetEncodings: Raw
        sent.extend(UPDATE);
        sent.extend(POINTER);
        sent.extend(KEY);
        sent.extend([6, 0, 0, 0, 0, 0, 0, 2, b'h', b'i']);
        sent.extend(UPDATE);
        // Split at every byte: frames cut messages anywhere.
        let mut forwarded = Vec::new();
        for byte in &sent {
            forwarded.extend(filter.feed(std::slice::from_ref(byte)).unwrap());
        }
        let mut expected = b"RFB 003.008\n\x01\x01".to_vec(); // shared
        expected.extend([2, 0, 0, 1, 0, 0, 0, 0]);
        expected.extend(UPDATE);
        expected.extend(UPDATE);
        assert_eq!(forwarded, expected);
    }

    #[test]
    fn an_unknown_message_or_security_type_is_refused() {
        let mut filter = ObserverFilter::new();
        let mut sent = HANDSHAKE.to_vec();
        sent.push(250);
        assert!(filter.feed(&sent).is_err());
        let mut filter = ObserverFilter::new();
        assert!(filter.feed(b"RFB 003.008\n\x02").is_err());
    }
}
