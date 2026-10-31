//! Markdown for the terminal (spec: cli, Display preferences:
//! `display.final_response_markdown = render`): the markup is replaced by
//! its terminal form — headings, emphasis and table headers by SGR
//! attributes, list markers by bullets, quotes by a bar, code blocks by
//! an indent, links by their text followed by the URL.
//!
//! Only attribute switches with their own "off" code are used (never a
//! full reset), so the theme colour the caller wraps the text in survives
//! inside. Without colour the text is the same minus the attributes;
//! inline code keeps its backticks there, its only marker.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

const BOLD: (&str, &str) = ("\x1b[1m", "\x1b[22m");
const ITALIC: (&str, &str) = ("\x1b[3m", "\x1b[23m");
const STRIKE: (&str, &str) = ("\x1b[9m", "\x1b[29m");
const HEADING: (&str, &str) = ("\x1b[1;4m", "\x1b[22;24m");
const CODE: (&str, &str) = ("\x1b[7m", "\x1b[27m");
const RULE: &str = "────────────────────";

/// `markdown` rendered for a terminal, with SGR attributes when `color`.
#[must_use]
pub fn render(markdown: &str, color: bool) -> String {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let mut out = Renderer {
        color,
        ..Renderer::default()
    };
    for event in Parser::new_ext(markdown, options) {
        out.event(event);
    }
    out.text.trim_end().to_owned()
}

#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // independent layout states
struct Renderer {
    text: String,
    color: bool,
    /// Open lists, innermost last: `None` bulleted, `Some(n)` the next number.
    lists: Vec<Option<u64>>,
    quotes: usize,
    code_block: bool,
    /// Open links: the URL and where their text starts in `text`.
    links: Vec<(String, usize)>,
    /// A block ended: the next one starts after a blank line.
    gap: bool,
    /// A list marker was just written: the item's first block goes on it.
    item_open: bool,
    /// The next cell is the first of its row.
    first_cell: bool,
}

impl Renderer {
    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text)
            | Event::InlineMath(text)
            | Event::DisplayMath(text)
            | Event::Html(text)
            | Event::InlineHtml(text) => self.write(&text),
            Event::Code(code) => self.code(&code),
            Event::FootnoteReference(name) => self.write(&format!("[^{name}]")),
            Event::SoftBreak | Event::HardBreak => self.write("\n"),
            Event::Rule => {
                self.block();
                self.write(RULE);
                self.gap = true;
            }
            Event::TaskListMarker(done) => self.write(if done { "[x] " } else { "[ ] " }),
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph | Tag::HtmlBlock | Tag::Table(_) => self.block(),
            Tag::Heading { .. } => {
                self.block();
                self.attr(HEADING.0);
            }
            Tag::BlockQuote(_) => {
                self.block();
                self.quotes += 1;
            }
            Tag::CodeBlock(kind) => self.code_block(&kind),
            Tag::List(first) => {
                if self.lists.is_empty() {
                    self.block();
                }
                self.lists.push(first);
            }
            Tag::Item => self.item(),
            Tag::TableHead => {
                self.row();
                self.attr(BOLD.0);
            }
            Tag::TableRow => self.row(),
            Tag::TableCell => self.cell(),
            Tag::Emphasis => self.attr(ITALIC.0),
            Tag::Strong => self.attr(BOLD.0),
            Tag::Strikethrough => self.attr(STRIKE.0),
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                self.links.push((dest_url.into_string(), self.text.len()));
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::HtmlBlock | TagEnd::Table => self.gap = true,
            TagEnd::Heading(_) => {
                self.attr(HEADING.1);
                self.gap = true;
            }
            TagEnd::BlockQuote(_) => {
                self.quotes -= 1;
                self.gap = true;
            }
            TagEnd::CodeBlock => {
                self.code_block = false;
                self.gap = true;
            }
            TagEnd::List(_) => {
                self.lists.pop();
                self.gap = self.lists.is_empty();
            }
            TagEnd::Item => self.gap = false,
            TagEnd::TableHead => {
                self.attr(BOLD.1);
                self.write("\n");
            }
            TagEnd::TableRow => self.write("\n"),
            TagEnd::Emphasis => self.attr(ITALIC.1),
            TagEnd::Strong => self.attr(BOLD.1),
            TagEnd::Strikethrough => self.attr(STRIKE.1),
            TagEnd::Link | TagEnd::Image => self.end_link(),
            _ => {}
        }
    }

    /// Starts a block: on the open list marker, else on a new line (after
    /// a blank one when the previous block asked for it).
    fn block(&mut self) {
        if std::mem::take(&mut self.item_open) {
            return;
        }
        if !self.text.is_empty() && !self.text.ends_with('\n') {
            self.text.push('\n');
        }
        if std::mem::take(&mut self.gap) && !self.text.is_empty() {
            self.text.push('\n');
        }
    }

    fn item(&mut self) {
        self.item_open = false;
        self.block();
        let depth = self.lists.len().saturating_sub(1);
        let marker = match self.lists.last_mut() {
            Some(Some(n)) => {
                *n += 1;
                format!("{}. ", *n - 1)
            }
            _ => "• ".to_owned(),
        };
        self.line_prefix();
        self.text.push_str(&"  ".repeat(depth));
        self.text.push_str(&marker);
        self.item_open = true;
    }

    fn code_block(&mut self, kind: &CodeBlockKind<'_>) {
        self.block();
        if let CodeBlockKind::Fenced(lang) = kind
            && !lang.is_empty()
        {
            self.write(&format!("({lang})\n"));
        }
        self.code_block = true;
    }

    fn row(&mut self) {
        self.block();
        self.first_cell = true;
    }

    fn cell(&mut self) {
        if !std::mem::take(&mut self.first_cell) {
            self.text.push_str(" │ ");
        }
    }

    fn code(&mut self, code: &str) {
        if self.color {
            self.attr(CODE.0);
            self.write(code);
            self.attr(CODE.1);
        } else {
            self.write(&format!("`{code}`"));
        }
    }

    fn end_link(&mut self) {
        let Some((url, start)) = self.links.pop() else {
            return;
        };
        let label = &self.text[start..];
        if !url.is_empty() && !label.contains(url.as_str()) {
            self.write(&format!(" <{url}>"));
        }
    }

    /// Writes `text`, starting each new line with the quote bars and the
    /// list or code indent.
    fn write(&mut self, text: &str) {
        self.item_open = false;
        for (i, line) in text.split('\n').enumerate() {
            if i > 0 {
                self.text.push('\n');
            }
            if line.is_empty() {
                continue;
            }
            if self.text.is_empty() || self.text.ends_with('\n') {
                self.line_prefix();
                self.text.push_str(&"  ".repeat(self.lists.len()));
            }
            self.text.push_str(line);
        }
    }

    fn line_prefix(&mut self) {
        self.text.push_str(&"│ ".repeat(self.quotes));
        if self.code_block {
            self.text.push_str("    ");
        }
    }

    fn attr(&mut self, code: &str) {
        if self.color {
            self.text.push_str(code);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn plain_text_is_unchanged() {
        assert_eq!(render("mock: hello", false), "mock: hello");
        assert_eq!(render("snake_case and a*b", true), "snake_case and a*b");
    }

    #[test]
    fn emphasis_becomes_attributes_or_disappears() {
        assert_eq!(
            render("a **b** *c*", true),
            "a \x1b[1mb\x1b[22m \x1b[3mc\x1b[23m"
        );
        assert_eq!(render("a **b** *c*", false), "a b c");
    }

    #[test]
    fn headings_lists_and_paragraphs_are_laid_out() {
        let text = "# Plan\n\nFirst:\n\n- one\n- two\n  1. deep\n\nDone.";
        assert_eq!(
            render(text, false),
            "Plan\n\nFirst:\n\n• one\n• two\n  1. deep\n\nDone."
        );
        assert!(render(text, true).starts_with("\x1b[1;4mPlan\x1b[22;24m\n\n"));
    }

    #[test]
    fn code_quotes_links_and_rules() {
        let text = "```rust\nlet x = 1;\n```\n\n> quoted\n\nsee [docs](https://x.dev) and <https://y.dev>\n\n---\n\nuse `cargo`";
        assert_eq!(
            render(text, false),
            "(rust)\n    let x = 1;\n\n│ quoted\n\nsee docs <https://x.dev> and https://y.dev\n\n────────────────────\n\nuse `cargo`"
        );
        assert!(render("use `cargo`", true).contains("\x1b[7mcargo\x1b[27m"));
    }

    #[test]
    fn tables_get_cell_separators() {
        let text = "| a | b |\n|---|---|\n| 1 | 2 |";
        assert_eq!(render(text, false), "a │ b\n1 │ 2");
    }

    #[test]
    fn task_lists_keep_their_state() {
        assert_eq!(
            render("- [x] done\n- [ ] todo", false),
            "• [x] done\n• [ ] todo"
        );
    }
}
