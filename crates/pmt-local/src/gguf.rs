//! GGUF metadata (spec: local-models, Model library: each model file's own
//! metadata chooses its window and memory placement). Only the key/value
//! header is read; large arrays (the tokenizer vocabulary) are skipped
//! without being loaded.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use serde::Serialize;

/// What Prometheus needs from a GGUF header.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Info {
    /// `general.architecture`.
    pub architecture: String,
    /// `general.name`, when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `<arch>.context_length`: the native window.
    pub context_length: Option<u64>,
    /// KV cache bytes per token (f16), when the attention shape is known.
    pub kv_bytes_per_token: Option<u64>,
}

/// Raw attention shape read from the header.
#[derive(Default)]
struct Shape {
    blocks: Option<u64>,
    heads: Option<u64>,
    kv_heads: Vec<u64>,
    key: Option<u64>,
    value: Option<u64>,
    embedding: Option<u64>,
    full_attention_interval: Option<u64>,
    sliding: Vec<bool>,
}

/// Reads the header of the GGUF file at `path`.
///
/// # Errors
///
/// A one-line reason when the file is not a readable GGUF file.
pub fn read(path: &Path) -> Result<Info, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut r = BufReader::new(file);
    let mut magic = [0; 4];
    r.read_exact(&mut magic).map_err(|e| e.to_string())?;
    if &magic != b"GGUF" {
        return Err("not a GGUF file".to_owned());
    }
    let version = u32_le(&mut r)?;
    if !(2..=3).contains(&version) {
        return Err(format!("unsupported GGUF version {version}"));
    }
    let _tensors = u64_le(&mut r)?;
    let count = u64_le(&mut r)?;
    let mut info = Info::default();
    let mut shape = Shape::default();
    for _ in 0..count {
        let key = string(&mut r)?;
        let kind = u32_le(&mut r)?;
        let value = value(&mut r, kind)?;
        record(&mut info, &mut shape, &key, value);
    }
    info.kv_bytes_per_token = shape.kv_bytes();
    Ok(info)
}

enum Value {
    Number(u64),
    Text(String),
    Numbers(Vec<u64>),
    Flags(Vec<bool>),
    Other,
}

fn record(info: &mut Info, shape: &mut Shape, key: &str, value: Value) {
    match (key, value) {
        ("general.architecture", Value::Text(arch)) => info.architecture = arch,
        ("general.name", Value::Text(name)) => info.name = Some(name),
        (key, value) => {
            let Some((_, field)) = key.split_once('.') else {
                return;
            };
            if !key.starts_with(&format!("{}.", info.architecture)) || info.architecture.is_empty()
            {
                return;
            }
            match (field, value) {
                ("context_length", Value::Number(n)) => info.context_length = Some(n),
                ("block_count", Value::Number(n)) => shape.blocks = Some(n),
                ("embedding_length", Value::Number(n)) => shape.embedding = Some(n),
                ("attention.head_count", Value::Number(n)) => shape.heads = Some(n),
                ("attention.head_count_kv", Value::Number(n)) => shape.kv_heads = vec![n],
                ("attention.head_count_kv", Value::Numbers(v)) => shape.kv_heads = v,
                ("attention.key_length", Value::Number(n)) => shape.key = Some(n),
                ("attention.value_length", Value::Number(n)) => shape.value = Some(n),
                ("full_attention_interval", Value::Number(n)) => {
                    shape.full_attention_interval = Some(n);
                }
                ("attention.sliding_window_pattern", Value::Flags(v)) => shape.sliding = v,
                _ => {}
            }
        }
    }
}

impl Shape {
    /// Bytes per token of the f16 KV cache of the full-attention layers
    /// (sliding-window and recurrent layers hold a bounded cache).
    fn kv_bytes(&self) -> Option<u64> {
        let blocks = self.blocks?;
        let head_dim = || Some(self.embedding? / self.heads?);
        let key = self.key.or_else(head_dim)?;
        let value = self.value.unwrap_or(key);
        let layer_heads = |layer: u64| -> u64 {
            let index = usize::try_from(layer).unwrap_or(usize::MAX);
            match self.kv_heads.as_slice() {
                [] => self.heads.unwrap_or(0),
                [one] => *one,
                many => many.get(index).copied().unwrap_or(0),
            }
        };
        let full = |layer: u64| -> bool {
            let index = usize::try_from(layer).unwrap_or(usize::MAX);
            if let Some(interval) = self.full_attention_interval.filter(|i| *i > 0) {
                return (layer + 1).is_multiple_of(interval);
            }
            !self.sliding.get(index).copied().unwrap_or(false)
        };
        let total: u64 = (0..blocks)
            .filter(|l| full(*l))
            .map(|l| layer_heads(l) * (key + value) * 2)
            .sum();
        (total > 0).then_some(total)
    }
}

fn value(r: &mut BufReader<File>, kind: u32) -> Result<Value, String> {
    Ok(match kind {
        0 | 1 | 7 => Value::Number(u64::from(byte(r)?)),
        2 | 3 => Value::Number(u64::from(u16::from_le_bytes(bytes(r)?))),
        4 | 5 => Value::Number(u64::from(u32_le(r)?)),
        6 => {
            skip(r, 4)?;
            Value::Other
        }
        10 | 11 => Value::Number(u64_le(r)?),
        12 => {
            skip(r, 8)?;
            Value::Other
        }
        8 => Value::Text(string(r)?),
        9 => array(r)?,
        other => return Err(format!("unknown GGUF value type {other}")),
    })
}

/// Arrays of small integers and flags are kept (per-layer head counts and
/// sliding-window patterns); everything else is skipped.
fn array(r: &mut BufReader<File>) -> Result<Value, String> {
    let kind = u32_le(r)?;
    let count = u64_le(r)?;
    let width: u64 = match kind {
        0 | 1 | 7 => 1,
        2 | 3 => 2,
        4..=6 => 4,
        10..=12 => 8,
        8 => {
            for _ in 0..count {
                let len = u64_le(r)?;
                skip(r, len)?;
            }
            return Ok(Value::Other);
        }
        other => return Err(format!("unsupported GGUF array type {other}")),
    };
    if count > 4096 {
        skip(r, count * width)?;
        return Ok(Value::Other);
    }
    match kind {
        7 => (0..count)
            .map(|_| byte(r).map(|b| b != 0))
            .collect::<Result<_, _>>()
            .map(Value::Flags),
        0 | 1 => (0..count)
            .map(|_| byte(r).map(u64::from))
            .collect::<Result<_, _>>()
            .map(Value::Numbers),
        4 | 5 => (0..count)
            .map(|_| u32_le(r).map(u64::from))
            .collect::<Result<_, _>>()
            .map(Value::Numbers),
        _ => {
            skip(r, count * width)?;
            Ok(Value::Other)
        }
    }
}

fn string(r: &mut BufReader<File>) -> Result<String, String> {
    let len = u64_le(r)?;
    if len > 1 << 20 {
        skip(r, len)?;
        return Ok(String::new());
    }
    let mut buf = vec![0; usize::try_from(len).map_err(|e| e.to_string())?];
    r.read_exact(&mut buf).map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn skip(r: &mut BufReader<File>, n: u64) -> Result<(), String> {
    let n = i64::try_from(n).map_err(|e| e.to_string())?;
    r.seek(SeekFrom::Current(n))
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn byte(r: &mut impl Read) -> Result<u8, String> {
    Ok(bytes::<1>(r)?[0])
}

fn u32_le(r: &mut impl Read) -> Result<u32, String> {
    Ok(u32::from_le_bytes(bytes(r)?))
}

fn u64_le(r: &mut impl Read) -> Result<u64, String> {
    Ok(u64::from_le_bytes(bytes(r)?))
}

fn bytes<const N: usize>(r: &mut impl Read) -> Result<[u8; N], String> {
    let mut buf = [0; N];
    r.read_exact(&mut buf)
        .map_err(|e: io::Error| e.to_string())?;
    Ok(buf)
}

/// Test support: a minimal GGUF header with string and integer keys.
#[doc(hidden)]
#[must_use]
pub fn fixture(arch: &str, context: u64, blocks: u32, kv_heads: u32, head_dim: u32) -> Vec<u8> {
    fn key(out: &mut Vec<u8>, k: &str) {
        out.extend((k.len() as u64).to_le_bytes());
        out.extend(k.as_bytes());
    }
    let mut out = b"GGUF".to_vec();
    out.extend(3u32.to_le_bytes());
    out.extend(0u64.to_le_bytes());
    out.extend(5u64.to_le_bytes());
    key(&mut out, "general.architecture");
    out.extend(8u32.to_le_bytes());
    key(&mut out, arch);
    for (k, v) in [
        ("block_count", u64::from(blocks)),
        ("attention.head_count_kv", u64::from(kv_heads)),
        ("attention.key_length", u64::from(head_dim)),
    ] {
        key(&mut out, &format!("{arch}.{k}"));
        out.extend(4u32.to_le_bytes());
        out.extend(u32::try_from(v).unwrap_or(u32::MAX).to_le_bytes());
    }
    key(&mut out, &format!("{arch}.context_length"));
    out.extend(10u32.to_le_bytes());
    out.extend(context.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::{fixture, read};

    #[test]
    fn the_window_and_kv_shape_are_read_from_the_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.gguf");
        std::fs::write(&path, fixture("llama", 131_072, 32, 8, 128)).unwrap();
        let info = read(&path).unwrap();
        assert_eq!(info.architecture, "llama");
        assert_eq!(info.context_length, Some(131_072));
        // 32 layers * 8 heads * (128 + 128) * 2 bytes.
        assert_eq!(info.kv_bytes_per_token, Some(131_072));
        std::fs::write(&path, b"not gguf").unwrap();
        assert!(read(&path).is_err());
    }
}
