//! Text decoding and inspection for the file tools (spec:
//! execution-environments, `read_file` encoding, conflict markers and path
//! suggestions; Write guards).

/// How a file's bytes were decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// UTF-8 (a leading byte-order mark is dropped).
    Utf8,
    /// UTF-16, little endian.
    Utf16Le,
    /// UTF-16, big endian.
    Utf16Be,
}

impl Encoding {
    /// The label reported to the model (`utf-16le`...).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Utf16Le => "utf-16le",
            Self::Utf16Be => "utf-16be",
        }
    }
}

/// Bytes inspected by the binary and UTF-16 heuristics.
const SAMPLE: usize = 8192;

/// Decodes `bytes` as text, or `None` for a binary file. UTF-16 is
/// recognised by its byte-order mark or, without one, by NUL bytes in
/// every other position (what ASCII-range UTF-16 text looks like); any
/// other NUL byte means binary. Invalid UTF-8 sequences in a NUL-free file
/// are replaced (`U+FFFD`), so legacy 8-bit text stays readable.
#[must_use]
pub fn decode(bytes: &[u8]) -> Option<(String, Encoding)> {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return Some((utf16(rest, u16::from_le_bytes), Encoding::Utf16Le));
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return Some((utf16(rest, u16::from_be_bytes), Encoding::Utf16Be));
    }
    if let Some(encoding) = utf16_without_bom(bytes) {
        let convert = match encoding {
            Encoding::Utf16Be => u16::from_be_bytes,
            _ => u16::from_le_bytes,
        };
        return Some((utf16(bytes, convert), encoding));
    }
    let sample = &bytes[..bytes.len().min(SAMPLE)];
    if sample.contains(&0) {
        return None;
    }
    let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    Some((String::from_utf8_lossy(body).into_owned(), Encoding::Utf8))
}

/// Whether `bytes` are a binary file (what [`decode`] refuses).
#[must_use]
pub fn is_binary(bytes: &[u8]) -> bool {
    decode(bytes).is_none()
}

/// UTF-16 without a byte-order mark: an even length and, in the sample,
/// zero bytes in nearly every odd (little endian) or even (big endian)
/// position and almost none in the other.
fn utf16_without_bom(bytes: &[u8]) -> Option<Encoding> {
    let sample = &bytes[..bytes.len().min(SAMPLE) & !1];
    if sample.len() < 4 || !bytes.len().is_multiple_of(2) {
        return None;
    }
    let pairs = sample.len() / 2;
    let zeros = |offset: usize| {
        sample
            .iter()
            .skip(offset)
            .step_by(2)
            .filter(|b| **b == 0)
            .count()
    };
    let (even, odd) = (zeros(0), zeros(1));
    let mostly = |n: usize| n * 10 >= pairs * 7;
    let rarely = |n: usize| n * 10 <= pairs;
    if mostly(odd) && rarely(even) {
        Some(Encoding::Utf16Le)
    } else if mostly(even) && rarely(odd) {
        Some(Encoding::Utf16Be)
    } else {
        None
    }
}

fn utf16(bytes: &[u8], convert: fn([u8; 2]) -> u16) -> String {
    let units = bytes
        .chunks_exact(2)
        .map(|pair| convert([pair[0], pair[1]]));
    char::decode_utf16(units)
        .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// Balanced merge-conflict blocks: a line starting `<<<<<<< ` later closed
/// by a line starting `>>>>>>> `.
#[must_use]
pub fn conflict_blocks<'a>(lines: impl IntoIterator<Item = &'a str>) -> usize {
    let mut open = false;
    let mut blocks = 0;
    for line in lines {
        if line.starts_with("<<<<<<< ") {
            open = true;
        } else if line.starts_with(">>>>>>> ") && open {
            open = false;
            blocks += 1;
        }
    }
    blocks
}

/// Edit distance between `a` and `b` (characters).
#[must_use]
pub fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut current = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(ca != *cb);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        previous = current;
    }
    previous[b.len()]
}

/// Similarity of two names in `0.0..=1.0` (1 = equal, case-insensitive).
#[must_use]
pub fn similarity(a: &str, b: &str) -> f64 {
    let (a, b) = (a.to_lowercase(), b.to_lowercase());
    let longest = a.chars().count().max(b.chars().count());
    if longest == 0 {
        return 1.0;
    }
    #[expect(clippy::cast_precision_loss, reason = "name lengths are small")]
    let score = 1.0 - levenshtein(&a, &b) as f64 / longest as f64;
    score
}

#[cfg(test)]
mod tests {
    use super::{Encoding, conflict_blocks, decode, is_binary, levenshtein};

    fn utf16le(text: &str, bom: bool) -> Vec<u8> {
        let mut bytes = if bom { vec![0xFF, 0xFE] } else { Vec::new() };
        bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        bytes
    }

    #[test]
    fn utf16_with_and_without_bom_decodes() {
        let text = "Installing component 1 of 3...\r\nDone.\r\n";
        assert_eq!(
            decode(&utf16le(text, true)),
            Some((text.into(), Encoding::Utf16Le))
        );
        assert_eq!(
            decode(&utf16le(text, false)),
            Some((text.into(), Encoding::Utf16Le))
        );
        let be: Vec<u8> = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
        assert_eq!(decode(&be), Some((text.into(), Encoding::Utf16Be)));
    }

    #[test]
    fn nul_bytes_mean_binary_and_text_does_not() {
        assert!(is_binary(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0"));
        assert!(!is_binary("plain text é".as_bytes()));
        assert_eq!(decode(b"\xEF\xBB\xBFbom").unwrap().0, "bom");
        assert_eq!(decode(b"latin \xE9").unwrap().0, "latin \u{FFFD}");
    }

    #[test]
    fn conflict_blocks_count_balanced_pairs_at_line_start() {
        let lines = [
            "<<<<<<< HEAD",
            "a",
            "=======",
            "b",
            ">>>>>>> topic",
            "  <<<<<<< not at start",
            ">>>>>>> unbalanced close",
            "<<<<<<< ours",
            ">>>>>>> theirs",
        ];
        assert_eq!(conflict_blocks(lines), 2);
    }

    #[test]
    fn edit_distance() {
        assert_eq!(levenshtein("conifg.rs", "config.rs"), 2);
        assert_eq!(levenshtein("", "abc"), 3);
    }
}
