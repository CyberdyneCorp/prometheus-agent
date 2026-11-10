//! Reply text as it is spoken (spec: voice-mode, Sentence-streamed spoken
//! replies): text deltas are buffered into complete sentences of at least
//! [`MIN_SENTENCE`] characters, with Markdown, code blocks, emoji and
//! reasoning blocks removed, so the first sentence can play while the
//! rest is still being generated and code is never read aloud.
//!
//! [`Sentences`] works on the stream: a fenced code block or reasoning
//! block is dropped as it arrives (nothing inside an open one is ever
//! emitted), complete lines are cleaned whole, and a line still being
//! written gives up its finished sentences early.

/// The shortest sentence spoken on its own; shorter ones join the next.
pub const MIN_SENTENCE: usize = 20;

/// Tags whose contents are the model's reasoning, never spoken.
const REASONING_TAGS: &[&str] = &["think", "thinking", "reasoning", "reflection"];

/// The streaming sentence buffer of one reply.
#[derive(Debug, Default)]
pub struct Sentences {
    /// Raw text not yet split into lines.
    raw: String,
    /// Inside a fenced code block: the fence that closes it.
    fence: Option<String>,
    /// Inside a reasoning block: the closing tag.
    reasoning: Option<String>,
    /// The end of an open reasoning block seen so far (where its closing
    /// tag may be arriving).
    hidden: String,
    /// Cleaned text not yet emitted.
    clean: String,
}

impl Sentences {
    /// A buffer at the start of a reply.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a text delta; returns the sentences now ready to speak.
    pub fn push(&mut self, delta: &str) -> Vec<String> {
        self.feed(delta);
        while let Some(end) = self.raw.find('\n') {
            let line: String = self.raw.drain(..=end).collect();
            self.line(line.trim_end_matches(['\n', '\r']), true);
        }
        self.partial();
        self.take(false)
    }

    /// Ends the reply: everything left that is speakable.
    pub fn finish(&mut self) -> Vec<String> {
        let rest = std::mem::take(&mut self.raw);
        if !rest.is_empty() {
            self.line(&rest, true);
        }
        self.take(true)
    }

    /// Finished sentences of the line still being written move on when
    /// nothing open (a fence, a reasoning block, a table row) could still
    /// change their meaning.
    fn partial(&mut self) {
        if self.fence.is_some() {
            return;
        }
        let start = self.raw.trim_start();
        if start.starts_with("```") || start.starts_with("~~~") || start.starts_with('|') {
            return;
        }
        if let Some(cut) = last_sentence_end(&self.raw) {
            let done: String = self.raw.drain(..cut).collect();
            self.line(&done, false);
        }
    }

    /// One line (or a finished piece of one when `whole` is false).
    fn line(&mut self, line: &str, whole: bool) {
        let rest = line;
        if let Some(fence) = &self.fence {
            if rest.trim_start().starts_with(fence.as_str()) {
                self.fence = None;
            }
            return;
        }
        let trimmed = rest.trim_start();
        if whole && (trimmed.starts_with("```") || trimmed.starts_with("~~~")) {
            let marker: String = trimmed
                .chars()
                .take_while(|c| *c == '`' || *c == '~')
                .collect();
            self.fence = Some(marker);
            return;
        }
        let cleaned = clean_line(rest);
        if cleaned.is_empty() {
            if whole {
                self.paragraph();
            }
            return;
        }
        if !self.clean.is_empty() && !self.clean.ends_with(' ') {
            self.clean.push(' ');
        }
        self.clean.push_str(&cleaned);
        if ends_sentence(&cleaned) {
            self.clean.push(' ');
        }
    }

    /// Moves `delta` into the raw text, minus reasoning blocks: an open
    /// one swallows everything until its closing tag.
    fn feed(&mut self, delta: &str) {
        let mut input = delta.to_owned();
        loop {
            if let Some(close) = self.reasoning.clone() {
                self.hidden.push_str(&input);
                let Some(at) = self.hidden.find(&close) else {
                    // Keep only what could be the start of the closing tag.
                    let mut keep = self.hidden.len().saturating_sub(close.len());
                    while !self.hidden.is_char_boundary(keep) {
                        keep += 1;
                    }
                    self.hidden.drain(..keep);
                    return;
                };
                input = self.hidden[at + close.len()..].to_owned();
                self.hidden.clear();
                self.reasoning = None;
            }
            self.raw.push_str(&input);
            let opened = REASONING_TAGS
                .iter()
                .filter_map(|tag| self.raw.find(&format!("<{tag}>")).map(|at| (at, *tag)))
                .min_by_key(|(at, _)| *at);
            let Some((at, tag)) = opened else { return };
            let block = self.raw.split_off(at);
            block[tag.len() + 2..].clone_into(&mut input);
            self.reasoning = Some(format!("</{tag}>"));
        }
    }

    /// A blank line ends whatever sentence was being built.
    fn paragraph(&mut self) {
        let text = self.clean.trim_end();
        if !text.is_empty() && !ends_sentence(text) {
            self.clean = format!("{text}. ");
        }
    }

    /// Splits off the complete sentences (all of the text at the end).
    fn take(&mut self, all: bool) -> Vec<String> {
        let mut out = Vec::new();
        let mut start = 0;
        let text = self.clean.clone();
        for (at, c) in text.char_indices() {
            let end = at + c.len_utf8();
            let boundary = matches!(c, '.' | '!' | '?' | '。' | '！' | '？')
                && text[end..].starts_with(char::is_whitespace);
            if !boundary {
                continue;
            }
            let sentence = text[start..end].trim();
            if sentence.chars().count() >= MIN_SENTENCE {
                out.push(sentence.to_owned());
                start = end;
            }
        }
        text[start..].trim_start().clone_into(&mut self.clean);
        if all {
            let rest = std::mem::take(&mut self.clean);
            let rest = rest.trim();
            if !rest.is_empty() {
                out.push(rest.to_owned());
            }
        }
        out
    }
}

/// The end of the last sentence of `text` followed by a space.
fn last_sentence_end(text: &str) -> Option<usize> {
    let mut found = None;
    for (at, c) in text.char_indices() {
        let end = at + c.len_utf8();
        if matches!(c, '.' | '!' | '?') && text[end..].starts_with(' ') {
            found = Some(end);
        }
    }
    found
}

fn ends_sentence(text: &str) -> bool {
    text.trim_end()
        .ends_with(['.', '!', '?', ':', ';', '。', '！', '？'])
}

/// `text` (a whole reply) as it is spoken.
#[must_use]
pub fn speakable(text: &str) -> String {
    let mut sentences = Sentences::new();
    let mut out = sentences.push(text);
    out.extend(sentences.finish());
    out.join(" ")
}

/// One line without Markdown or emoji.
fn clean_line(line: &str) -> String {
    let mut text = line.trim();
    // Block markers: headings, quotes, list bullets, numbered items.
    text = text.trim_start_matches('#').trim_start();
    while let Some(rest) = text.strip_prefix('>') {
        text = rest.trim_start();
    }
    for bullet in ["- ", "* ", "+ ", "• "] {
        if let Some(rest) = text.strip_prefix(bullet) {
            text = rest;
        }
    }
    if is_rule(text) {
        return String::new();
    }
    let text = if text.starts_with('|') {
        table_row(text)
    } else {
        text.to_owned()
    };
    let text = links(&text);
    let text: String = text
        .chars()
        .filter(|c| !matches!(c, '*' | '_' | '`' | '~') && !is_emoji(*c))
        .collect();
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `---`, `***`, `___` and table separators (`|---|:--:|`).
fn is_rule(text: &str) -> bool {
    text.len() >= 3
        && text
            .chars()
            .all(|c| matches!(c, '-' | '*' | '_' | '|' | ':' | ' '))
}

/// A table row read as its cells, comma separated.
fn table_row(text: &str) -> String {
    text.split('|')
        .map(str::trim)
        .filter(|cell| !cell.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// `[text](url)` becomes `text`; `![alt](url)` disappears.
fn links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let image = open > 0 && rest[..open].ends_with('!');
        let Some(close) = rest[open..].find("](").map(|c| open + c) else {
            break;
        };
        let Some(end) = rest[close..].find(')').map(|e| close + e) else {
            break;
        };
        let prefix = if image {
            &rest[..open - 1]
        } else {
            &rest[..open]
        };
        out.push_str(prefix);
        if !image {
            out.push_str(&rest[open + 1..close]);
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Emoji and pictographs (and the joiners and selectors that build them).
fn is_emoji(c: char) -> bool {
    matches!(u32::from(c),
        0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0x2B00..=0x2BFF | 0xFE0F | 0x200D | 0x20E3
            | 0xE0020..=0xE007F)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_and_emoji_are_not_spoken() {
        assert_eq!(
            speakable(
                "## Result 🎉\n\nThe **build** passed, see [the log](https://x.y/z).\n- `cargo` is happy ✅"
            ),
            "Result. The build passed, see the log. cargo is happy"
        );
    }

    #[test]
    fn reasoning_blocks_are_dropped_even_across_deltas() {
        let mut s = Sentences::new();
        let mut out = s.push("<think>The user wants");
        out.extend(s.push(" the time. Easy.</think>It is three o'clock right now. "));
        out.extend(s.finish());
        assert_eq!(out, ["It is three o'clock right now."]);
    }

    #[test]
    fn short_sentences_join_the_next() {
        let mut s = Sentences::new();
        assert!(s.push("Yes. Sure. ").is_empty());
        let out = s.push("I will do that right away. ");
        assert_eq!(out, ["Yes. Sure. I will do that right away."]);
    }

    #[test]
    fn a_table_is_read_by_its_cells() {
        assert_eq!(
            speakable("| name | size |\n|---|---|\n| a.txt | 3 KB |"),
            "name, size. a.txt, 3 KB"
        );
    }
}
