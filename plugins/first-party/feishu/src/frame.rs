//! The frames of Feishu's long connection (the `pbbp2` protobuf the
//! official SDKs speak), by hand: `Frame {SeqID = 1, LogID = 2, service =
//! 3, method = 4, repeated Header headers = 5, payload_encoding = 6,
//! payload_type = 7, payload = 8}` with `Header {key = 1, value = 2}`.
//! `method` 0 is a control frame (`type` header `ping`/`pong`), 1 a data
//! frame (`type` `event` or `card`) whose payload is the event JSON, split
//! into `sum` parts when large.

/// Control frames.
pub const CONTROL: u64 = 0;
/// Data frames.
pub const DATA: u64 = 1;

/// One frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frame {
    /// `SeqID`.
    pub seq_id: u64,
    /// `LogID`.
    pub log_id: u64,
    /// `service` (the connection's service id).
    pub service: u64,
    /// [`CONTROL`] or [`DATA`].
    pub method: u64,
    /// Headers, in order.
    pub headers: Vec<(String, String)>,
    /// The payload.
    pub payload: Vec<u8>,
}

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

fn int_field(n: u64, value: u64, out: &mut Vec<u8>) {
    varint(n << 3, out);
    varint(value, out);
}

fn bytes_field(n: u64, value: &[u8], out: &mut Vec<u8>) {
    varint((n << 3) | 2, out);
    varint(value.len() as u64, out);
    out.extend_from_slice(value);
}

/// A field's value: a varint or length-delimited bytes.
enum Val<'a> {
    Int(u64),
    Bytes(&'a [u8]),
}

/// The fields of one message, in wire order.
fn fields(data: &[u8]) -> Option<Vec<(u64, Val<'_>)>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let tag = read_varint(data, &mut at)?;
        let value = match tag & 7 {
            0 => Val::Int(read_varint(data, &mut at)?),
            2 => {
                let len = usize::try_from(read_varint(data, &mut at)?).ok()?;
                let bytes = data.get(at..at.checked_add(len)?)?;
                at += len;
                Val::Bytes(bytes)
            }
            _ => return None,
        };
        out.push((tag >> 3, value));
    }
    Some(out)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

impl Frame {
    /// Decodes one WebSocket binary frame.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut frame = Self::default();
        for (n, value) in fields(bytes)? {
            match (n, value) {
                (1, Val::Int(v)) => frame.seq_id = v,
                (2, Val::Int(v)) => frame.log_id = v,
                (3, Val::Int(v)) => frame.service = v,
                (4, Val::Int(v)) => frame.method = v,
                (5, Val::Bytes(b)) => {
                    let (mut key, mut value) = (String::new(), String::new());
                    for (m, v) in fields(b)? {
                        match (m, v) {
                            (1, Val::Bytes(k)) => key = text(k),
                            (2, Val::Bytes(v)) => value = text(v),
                            _ => {}
                        }
                    }
                    frame.headers.push((key, value));
                }
                (8, Val::Bytes(b)) => frame.payload = b.to_vec(),
                _ => {}
            }
        }
        Some(frame)
    }

    /// Encodes it.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        int_field(1, self.seq_id, &mut out);
        int_field(2, self.log_id, &mut out);
        int_field(3, self.service, &mut out);
        int_field(4, self.method, &mut out);
        for (key, value) in &self.headers {
            let mut header = Vec::new();
            bytes_field(1, key.as_bytes(), &mut header);
            bytes_field(2, value.as_bytes(), &mut header);
            bytes_field(5, &header, &mut out);
        }
        if !self.payload.is_empty() {
            bytes_field(8, &self.payload, &mut out);
        }
        out
    }

    /// The header `key` (empty when absent).
    #[must_use]
    pub fn header(&self, key: &str) -> &str {
        self.headers
            .iter()
            .find(|(k, _)| k == key)
            .map_or("", |(_, v)| v.as_str())
    }

    /// A ping on connection `service`.
    #[must_use]
    pub fn ping(service: u64) -> Self {
        Self {
            service,
            method: CONTROL,
            headers: vec![("type".into(), "ping".into())],
            ..Self::default()
        }
    }

    /// The acknowledgement of this data frame: the same frame answering
    /// `{"code": 200}`.
    #[must_use]
    pub fn ack(&self) -> Self {
        let mut headers = self.headers.clone();
        headers.push(("biz_rt".into(), "0".into()));
        Self {
            headers,
            payload: br#"{"code":200,"headers":{},"data":""}"#.to_vec(),
            ..self.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_with_headers_and_payload() {
        let frame = Frame {
            seq_id: 3,
            log_id: 9,
            service: 42,
            method: DATA,
            headers: vec![("type".into(), "event".into()), ("sum".into(), "1".into())],
            payload: b"{\"a\":1}".to_vec(),
        };
        let decoded = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded, frame);
        assert_eq!(decoded.header("type"), "event");
        assert_eq!(decoded.header("missing"), "");
        let ack = decoded.ack();
        assert_eq!(ack.header("biz_rt"), "0");
        assert!(String::from_utf8_lossy(&ack.payload).contains("200"));
        assert!(Frame::decode(&[0x0a, 0x05, 1]).is_none());
    }
}
