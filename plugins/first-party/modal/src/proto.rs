//! The little of protobuf and gRPC the Modal API needs: building request
//! messages field by field, reading response fields, and gRPC's
//! length-prefixed framing. Field numbers come from Modal's published
//! `modal_proto/api.proto` (package `modal.client`).

/// A message being built.
#[derive(Default)]
pub struct Msg(Vec<u8>);

impl Msg {
    pub fn new() -> Self {
        Self::default()
    }

    fn key(&mut self, field: u32, wire: u8) {
        varint(&mut self.0, (u64::from(field) << 3) | u64::from(wire));
    }

    pub fn uint(mut self, field: u32, value: u64) -> Self {
        self.key(field, 0);
        varint(&mut self.0, value);
        self
    }

    pub fn flag(self, field: u32, value: bool) -> Self {
        self.uint(field, u64::from(value))
    }

    pub fn float(mut self, field: u32, value: f32) -> Self {
        self.key(field, 5);
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn bytes(mut self, field: u32, value: &[u8]) -> Self {
        self.key(field, 2);
        varint(&mut self.0, value.len() as u64);
        self.0.extend_from_slice(value);
        self
    }

    pub fn text(self, field: u32, value: &str) -> Self {
        self.bytes(field, value.as_bytes())
    }

    pub fn texts<S: AsRef<str>>(self, field: u32, values: &[S]) -> Self {
        values.iter().fold(self, |m, v| m.text(field, v.as_ref()))
    }

    pub fn message(self, field: u32, value: Msg) -> Self {
        self.bytes(field, &value.0)
    }

    pub fn finish(self) -> Vec<u8> {
        self.0
    }
}

fn varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// One decoded field value.
#[derive(Debug, Clone)]
pub enum Value {
    Varint(u64),
    Bytes(Vec<u8>),
    /// A fixed32/fixed64 value (no field the plugin reads has one).
    Fixed,
}

/// A decoded message: its fields in wire order.
#[derive(Debug, Clone, Default)]
pub struct Fields(Vec<(u32, Value)>);

impl Fields {
    pub fn parse(mut bytes: &[u8]) -> Result<Self, String> {
        let bad = || "malformed protobuf message".to_owned();
        let mut fields = Vec::new();
        while !bytes.is_empty() {
            let key = read_varint(&mut bytes).ok_or_else(bad)?;
            let field = u32::try_from(key >> 3).map_err(|_| bad())?;
            let value = match key & 7 {
                0 => Value::Varint(read_varint(&mut bytes).ok_or_else(bad)?),
                1 => {
                    bytes = bytes.get(8..).ok_or_else(bad)?;
                    Value::Fixed
                }
                2 => {
                    let len = usize::try_from(read_varint(&mut bytes).ok_or_else(bad)?)
                        .map_err(|_| bad())?;
                    let (head, rest) = bytes.split_at_checked(len).ok_or_else(bad)?;
                    bytes = rest;
                    Value::Bytes(head.to_vec())
                }
                5 => {
                    bytes = bytes.get(4..).ok_or_else(bad)?;
                    Value::Fixed
                }
                _ => return Err(bad()),
            };
            fields.push((field, value));
        }
        Ok(Self(fields))
    }

    fn all(&self, field: u32) -> impl Iterator<Item = &Value> {
        self.0
            .iter()
            .filter(move |(f, _)| *f == field)
            .map(|(_, v)| v)
    }

    /// The last value of a scalar field, as protobuf reads it.
    fn last(&self, field: u32) -> Option<&Value> {
        self.all(field).last()
    }

    pub fn has(&self, field: u32) -> bool {
        self.last(field).is_some()
    }

    pub fn uint(&self, field: u32) -> u64 {
        match self.last(field) {
            Some(Value::Varint(v)) => *v,
            _ => 0,
        }
    }

    /// An `int32` (negative values are sign-extended varints).
    pub fn int(&self, field: u32) -> Option<i32> {
        match self.last(field) {
            Some(Value::Varint(v)) => Some(*v as i64 as i32),
            _ => None,
        }
    }

    pub fn flag(&self, field: u32) -> bool {
        self.uint(field) != 0
    }

    pub fn bytes(&self, field: u32) -> Vec<u8> {
        match self.last(field) {
            Some(Value::Bytes(b)) => b.clone(),
            _ => Vec::new(),
        }
    }

    pub fn text(&self, field: u32) -> String {
        String::from_utf8_lossy(&self.bytes(field)).into_owned()
    }

    pub fn message(&self, field: u32) -> Option<Fields> {
        match self.last(field) {
            Some(Value::Bytes(b)) => Fields::parse(b).ok(),
            _ => None,
        }
    }

    pub fn messages(&self, field: u32) -> Vec<Fields> {
        self.all(field)
            .filter_map(|v| match v {
                Value::Bytes(b) => Fields::parse(b).ok(),
                _ => None,
            })
            .collect()
    }
}

fn read_varint(bytes: &mut &[u8]) -> Option<u64> {
    let mut value = 0_u64;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = bytes.split_first()?;
        *bytes = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// `message` as one gRPC frame (uncompressed).
pub fn frame(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len() + 5);
    out.push(0);
    out.extend_from_slice(&(message.len() as u32).to_be_bytes());
    out.extend_from_slice(message);
    out
}

/// The messages of a gRPC response body (trailer frames skipped).
pub fn unframe(mut body: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let mut messages = Vec::new();
    while !body.is_empty() {
        let (head, rest) = body
            .split_at_checked(5)
            .ok_or("truncated gRPC frame header")?;
        let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
        let (data, rest) = rest.split_at_checked(len).ok_or("truncated gRPC frame")?;
        body = rest;
        match head[0] {
            0 => messages.push(data.to_vec()),
            0x80 => {}
            _ => return Err("compressed gRPC frames are not supported".to_owned()),
        }
    }
    Ok(messages)
}

/// `grpc-message` text (percent-encoded on the wire).
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = text.get(i + 1..i + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
