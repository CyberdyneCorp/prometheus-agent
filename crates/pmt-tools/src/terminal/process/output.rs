//! The rolling output buffer of one background process.

use super::BUFFER_CHARS;

/// The last [`BUFFER_CHARS`] characters a process printed.
#[derive(Debug, Default)]
pub(super) struct Output {
    pub(super) text: String,
    chars: usize,
    /// Characters ever received.
    pub(super) total: usize,
    /// `total` at the last `poll`.
    pub(super) polled: usize,
    /// `total` at the last heartbeat.
    pub(super) beat: usize,
    /// An incomplete UTF-8 sequence at the end of the last chunk.
    carry: Vec<u8>,
}

impl Output {
    /// Appends `bytes` and returns the text they completed (an incomplete
    /// UTF-8 sequence at the very end is held back for the next chunk).
    pub(super) fn push(&mut self, bytes: &[u8]) -> String {
        self.carry.extend_from_slice(bytes);
        let valid = match std::str::from_utf8(&self.carry) {
            Err(err) if err.error_len().is_none() => err.valid_up_to(),
            _ => self.carry.len(),
        };
        let chunk: Vec<u8> = self.carry.drain(..valid).collect();
        let text = String::from_utf8_lossy(&chunk).into_owned();
        let added = text.chars().count();
        self.text.push_str(&text);
        self.chars += added;
        self.total += added;
        if self.chars > BUFFER_CHARS {
            let excess = self.chars - BUFFER_CHARS;
            let cut = self
                .text
                .char_indices()
                .nth(excess)
                .map_or(self.text.len(), |(i, _)| i);
            self.text.drain(..cut);
            self.chars = BUFFER_CHARS;
        }
        text
    }

    /// Output received after character `mark` (as much as is still kept).
    pub(super) fn since(&self, mark: usize) -> &str {
        let first_kept = self.total - self.chars;
        let skip = mark.saturating_sub(first_kept);
        let at = self
            .text
            .char_indices()
            .nth(skip)
            .map_or(self.text.len(), |(i, _)| i);
        &self.text[at..]
    }
}

#[cfg(test)]
mod tests {
    use super::Output;

    #[test]
    fn the_buffer_keeps_the_last_characters_and_utf8_split_across_chunks() {
        let mut output = Output::default();
        let bytes = "é".as_bytes();
        assert_eq!(output.push(&bytes[..1]), "");
        assert_eq!(output.push(&bytes[1..]), "é");
        assert_eq!(output.text, "é");
        output.push("x".repeat(super::BUFFER_CHARS).as_bytes());
        assert_eq!(output.chars, super::BUFFER_CHARS);
        assert!(output.text.chars().all(|c| c == 'x'));
        assert_eq!(output.since(output.total - 3), "xxx");
    }
}
