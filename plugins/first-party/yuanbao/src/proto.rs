//! The protobuf wire format of the Yuanbao gateway, by hand (varints,
//! length-delimited fields): `ConnMsg {Head head = 1; bytes data = 2}`
//! frames, with `Head {cmd_type = 1, cmd = 2, seq_no = 3, msg_id = 4,
//! module = 5, need_ack = 6, status = 10}`, and the business messages
//! they carry.

/// A decoded field value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Val {
    /// A varint (or fixed-width integer).
    Int(u64),
    /// Length-delimited bytes (strings, messages).
    Bytes(Vec<u8>),
}

/// The fields of one message, in wire order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fields(pub Vec<(u32, Val)>);

fn varint(mut value: u64, out: &mut Vec<u8>) {
    loop {
        #[allow(clippy::cast_possible_truncation)]
        let bits = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(bits);
            return;
        }
        out.push(bits | 0x80);
    }
}

fn read_varint(data: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *data.get(*at)?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

impl Fields {
    /// Parses `data`; `None` when it is not a protobuf message.
    #[must_use]
    pub fn parse(data: &[u8]) -> Option<Self> {
        let mut fields = Vec::new();
        let mut at = 0;
        while at < data.len() {
            let tag = read_varint(data, &mut at)?;
            let number = u32::try_from(tag >> 3).ok()?;
            let value = match tag & 7 {
                0 => Val::Int(read_varint(data, &mut at)?),
                1 | 5 => {
                    let width = if tag & 7 == 1 { 8 } else { 4 };
                    let bytes = data.get(at..at + width)?;
                    at += width;
                    let mut wide = [0u8; 8];
                    wide[..width].copy_from_slice(bytes);
                    Val::Int(u64::from_le_bytes(wide))
                }
                2 => {
                    let len = usize::try_from(read_varint(data, &mut at)?).ok()?;
                    let bytes = data.get(at..at.checked_add(len)?)?.to_vec();
                    at += len;
                    Val::Bytes(bytes)
                }
                _ => return None,
            };
            fields.push((number, value));
        }
        Some(Self(fields))
    }

    /// The first bytes of field `n`.
    #[must_use]
    pub fn bytes(&self, n: u32) -> Option<&[u8]> {
        self.0.iter().find_map(|(f, v)| match v {
            Val::Bytes(b) if *f == n => Some(b.as_slice()),
            _ => None,
        })
    }

    /// Field `n` as text (empty when absent).
    #[must_use]
    pub fn string(&self, n: u32) -> String {
        self.bytes(n)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default()
    }

    /// Field `n` as an integer (0 when absent).
    #[must_use]
    pub fn int(&self, n: u32) -> u64 {
        self.0
            .iter()
            .find_map(|(f, v)| match v {
                Val::Int(i) if *f == n => Some(*i),
                _ => None,
            })
            .unwrap_or(0)
    }

    /// Every occurrence of field `n` parsed as a message.
    #[must_use]
    pub fn messages(&self, n: u32) -> Vec<Self> {
        self.0
            .iter()
            .filter_map(|(f, v)| match v {
                Val::Bytes(b) if *f == n => Self::parse(b),
                _ => None,
            })
            .collect()
    }

    /// Field `n` parsed as a message (empty when absent).
    #[must_use]
    pub fn message(&self, n: u32) -> Self {
        self.bytes(n).and_then(Self::parse).unwrap_or_default()
    }
}

/// Builds a message field by field.
#[derive(Debug, Clone, Default)]
pub struct Writer(pub Vec<u8>);

impl Writer {
    /// A varint field (skipped when zero, as proto3 does).
    #[must_use]
    pub fn int(mut self, n: u32, value: u64) -> Self {
        if value != 0 {
            varint(u64::from(n) << 3, &mut self.0);
            varint(value, &mut self.0);
        }
        self
    }

    /// A bytes field (skipped when empty).
    #[must_use]
    pub fn bytes(mut self, n: u32, value: &[u8]) -> Self {
        if !value.is_empty() {
            varint((u64::from(n) << 3) | 2, &mut self.0);
            varint(value.len() as u64, &mut self.0);
            self.0.extend_from_slice(value);
        }
        self
    }

    /// A string field (skipped when empty).
    #[must_use]
    pub fn string(self, n: u32, value: &str) -> Self {
        self.bytes(n, value.as_bytes())
    }

    /// A string field written even when empty.
    #[must_use]
    pub fn required(mut self, n: u32, value: &str) -> Self {
        varint((u64::from(n) << 3) | 2, &mut self.0);
        varint(value.len() as u64, &mut self.0);
        self.0.extend_from_slice(value.as_bytes());
        self
    }

    /// A nested message.
    #[must_use]
    pub fn message(self, n: u32, value: &Self) -> Self {
        self.bytes(n, &value.0)
    }
}

/// `ConnMsg.Head.cmd_type` values.
pub mod cmd_type {
    /// An upstream request.
    pub const REQUEST: u64 = 0;
    /// The answer to one.
    pub const RESPONSE: u64 = 1;
    /// A downstream push.
    pub const PUSH: u64 = 2;
    /// Acknowledges a push.
    pub const PUSH_ACK: u64 = 3;
}

/// A decoded frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frame {
    /// `cmd_type`.
    pub cmd_type: u64,
    /// `cmd`.
    pub cmd: String,
    /// `msg_id` (a request's id; answers carry it back).
    pub msg_id: String,
    /// `module`.
    pub module: String,
    /// `need_ack`.
    pub need_ack: bool,
    /// `status` (non-zero: refused).
    pub status: u64,
    /// The business payload.
    pub data: Vec<u8>,
}

impl Frame {
    /// Decodes one WebSocket frame.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let conn = Fields::parse(bytes)?;
        let head = conn.message(1);
        Some(Self {
            cmd_type: head.int(1),
            cmd: head.string(2),
            msg_id: head.string(4),
            module: head.string(5),
            need_ack: head.int(6) != 0,
            status: head.int(10),
            data: conn.bytes(2).unwrap_or_default().to_vec(),
        })
    }

    /// Encodes it with sequence number `seq`.
    #[must_use]
    pub fn encode(&self, seq: u64) -> Vec<u8> {
        let head = Writer::default()
            .int(1, self.cmd_type)
            .string(2, &self.cmd)
            .int(3, seq)
            .string(4, &self.msg_id)
            .string(5, &self.module)
            .int(6, u64::from(self.need_ack));
        Writer::default().message(1, &head).bytes(2, &self.data).0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let frame = Frame {
            cmd_type: cmd_type::REQUEST,
            cmd: "send_c2c_message".into(),
            msg_id: "c2c_1".into(),
            module: "yuanbao_openclaw_proxy".into(),
            need_ack: false,
            status: 0,
            data: Writer::default().required(2, "user").int(4, 300).0,
        };
        let decoded = Frame::decode(&frame.encode(7)).unwrap();
        assert_eq!(decoded, frame);
        let body = Fields::parse(&decoded.data).unwrap();
        assert_eq!((body.string(2), body.int(4)), ("user".to_owned(), 300));
        assert!(Fields::parse(&[0x0a, 0x05, 1]).is_none());
    }
}
