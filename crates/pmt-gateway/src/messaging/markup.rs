//! Markup conversion per platform (spec: platform-adapters, Markup
//! conversion per platform): the agent writes Markdown; each descriptor's
//! `markup` (and, for `platform`, the platform) decides what a part is
//! sent as. Text inside code spans and fences is never altered: only
//! escaped where the dialect requires it. A platform that refuses the
//! result gets the plain rendering through the send-retry `formatting`
//! path.
//!
//! - `plain`: `[t](u)` becomes `t (u)` (SMS, LINE, IRC, ntfy).
//! - `markdown`: sent as written (Discord; Telegram rich messages, whose
//!   adapter falls back to `MarkdownV2` itself when Telegram refuses one).
//! - `html`: Matrix HTML.
//! - `platform`: Telegram `MarkdownV2` (small tables as bullet groups,
//!   larger ones as a fenced block), Slack
//!   `mrkdwn`, `WhatsApp` style; Signal keeps Markdown here and the adapter
//!   sends native style ranges ([`signal_styles`]).

use super::adapter::{Adapter, Outgoing};
use super::descriptor::{Descriptor, Markup};

/// Tables up to this many data rows and columns render as bullet groups
/// on Telegram; larger ones as a fenced block.
const SMALL_TABLE: (usize, usize) = (10, 4);

/// One inline piece of a line.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Inline {
    Text(String),
    Code(String),
    Bold(Vec<Inline>),
    Italic(Vec<Inline>),
    Strike(Vec<Inline>),
    Spoiler(Vec<Inline>),
    Link(Vec<Inline>, String),
}

/// One block of a message.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Block {
    Line(Vec<Inline>),
    Heading(usize, Vec<Inline>),
    Fence(String, String),
    Table(Vec<Vec<String>>),
}

/// Builds an emphasis piece from its inner pieces.
type Make = fn(Vec<Inline>) -> Inline;

/// The delimiters of emphasis, longest first.
const MARKS: &[(&str, Make)] = &[
    ("**", Inline::Bold),
    ("__", Inline::Bold),
    ("~~", Inline::Strike),
    ("||", Inline::Spoiler),
    ("*", Inline::Italic),
    ("_", Inline::Italic),
];

fn push_text(out: &mut Vec<Inline>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(Inline::Text(last)) = out.last_mut() {
        last.push_str(text);
    } else {
        out.push(Inline::Text(text.to_owned()));
    }
}

/// A `[text](url)` at the start of `rest`: its text, URL and length.
fn link(rest: &str) -> Option<(&str, &str, usize)> {
    let close = rest.find("](")?;
    let text = &rest[1..close];
    let after = &rest[close + 2..];
    // URLs may hold balanced parentheses (`.../Foo_(bar)`).
    let mut depth = 0usize;
    let end = after.char_indices().find_map(|(i, c)| match c {
        '(' => {
            depth += 1;
            None
        }
        ')' if depth == 0 => Some(i),
        ')' => {
            depth -= 1;
            None
        }
        _ => None,
    })?;
    let url = &after[..end];
    (!url.contains(char::is_whitespace) && !text.contains('\n')).then_some((
        text,
        url,
        close + 2 + end + 1,
    ))
}

/// Emphasis opening at the start of `rest`: its constructor, inner text
/// and length.
fn emphasis(rest: &str) -> Option<(Make, &str, usize)> {
    MARKS.iter().find_map(|(mark, make)| {
        let body = rest.strip_prefix(mark)?;
        if body.starts_with(char::is_whitespace) {
            return None;
        }
        let end = body.find(mark)?;
        let inner = &body[..end];
        let single_underscore = *mark == "_";
        // `snake_case` words are not italics.
        let word_after = body[end + mark.len()..]
            .chars()
            .next()
            .is_some_and(char::is_alphanumeric);
        let refused = inner.is_empty()
            || inner.ends_with(char::is_whitespace)
            || (single_underscore && word_after);
        (!refused).then_some((*make, inner, mark.len() * 2 + inner.len()))
    })
}

/// The inline pieces of `text`.
fn inline(text: &str) -> Vec<Inline> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if c == '`'
            && let Some(end) = rest[1..].find('`')
        {
            out.push(Inline::Code(rest[1..=end].to_owned()));
            rest = &rest[end + 2..];
            continue;
        }
        if c == '['
            && let Some((label, url, len)) = link(rest)
        {
            out.push(Inline::Link(inline(label), url.to_owned()));
            rest = &rest[len..];
            continue;
        }
        if matches!(c, '*' | '_' | '~' | '|')
            && let Some((make, inner, len)) = emphasis(rest)
        {
            out.push(make(inline(inner)));
            rest = &rest[len..];
            continue;
        }
        push_text(&mut out, &rest[..c.len_utf8()]);
        rest = &rest[c.len_utf8()..];
    }
    out
}

fn table_row(line: &str) -> Vec<String> {
    line.trim()
        .trim_start_matches('|')
        .trim_end_matches('|')
        .split('|')
        .map(|cell| cell.trim().to_owned())
        .collect()
}

fn is_separator(line: &str) -> bool {
    let line = line.trim();
    line.starts_with('|')
        && line.contains('-')
        && line.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// The blocks of `text`: fences, tables, headings and lines.
fn blocks(text: &str) -> Vec<Block> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(lang) = line.trim_start().strip_prefix("```") {
            let end = lines[i + 1..]
                .iter()
                .position(|l| l.trim_start().starts_with("```"))
                .map_or(lines.len(), |p| i + 1 + p);
            let body = lines[i + 1..end.min(lines.len())].join("\n");
            out.push(Block::Fence(lang.trim().to_owned(), body));
            i = end + 1;
            continue;
        }
        if line.trim_start().starts_with('|') && lines.get(i + 1).is_some_and(|l| is_separator(l)) {
            let mut rows = vec![table_row(line)];
            i += 2;
            while i < lines.len() && lines[i].trim_start().starts_with('|') {
                rows.push(table_row(lines[i]));
                i += 1;
            }
            out.push(Block::Table(rows));
            continue;
        }
        let hashes = line.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && line[hashes..].starts_with(' ') {
            out.push(Block::Heading(hashes, inline(line[hashes..].trim())));
        } else {
            out.push(Block::Line(inline(line)));
        }
        i += 1;
    }
    out
}

/// How one target renders each piece.
trait Dialect {
    fn text(&self, text: &str) -> String;
    fn code(&self, code: &str) -> String;
    fn fence(&self, lang: &str, body: &str) -> String;
    fn bold(&self, inner: String) -> String;
    fn italic(&self, inner: String) -> String;
    fn strike(&self, inner: String) -> String;
    fn spoiler(&self, inner: String) -> String;
    fn link(&self, label: String, url: &str) -> String;
    fn heading(&self, level: usize, inner: String) -> String;
    fn table(&self, rows: &[Vec<String>]) -> String;
    fn line_break(&self) -> &'static str {
        "\n"
    }

    fn pieces(&self, pieces: &[Inline]) -> String {
        pieces.iter().map(|p| self.piece(p)).collect()
    }

    fn piece(&self, piece: &Inline) -> String {
        match piece {
            Inline::Text(t) => self.text(t),
            Inline::Code(c) => self.code(c),
            Inline::Bold(p) => self.bold(self.pieces(p)),
            Inline::Italic(p) => self.italic(self.pieces(p)),
            Inline::Strike(p) => self.strike(self.pieces(p)),
            Inline::Spoiler(p) => self.spoiler(self.pieces(p)),
            Inline::Link(p, url) => self.link(self.pieces(p), url),
        }
    }

    fn render(&self, text: &str) -> String {
        let rendered: Vec<String> = blocks(text)
            .iter()
            .map(|block| match block {
                Block::Line(p) => self.pieces(p),
                Block::Heading(level, p) => self.heading(*level, self.pieces(p)),
                Block::Fence(lang, body) => self.fence(lang, body),
                Block::Table(rows) => self.table(rows),
            })
            .collect();
        rendered.join(self.line_break())
    }
}

/// The raw Markdown of a table (inside a code block).
fn raw_table(rows: &[Vec<String>]) -> String {
    let mut lines: Vec<String> = rows
        .iter()
        .map(|r| format!("| {} |", r.join(" | ")))
        .collect();
    if let Some(header) = rows.first() {
        let separator = vec!["---"; header.len()].join(" | ");
        lines.insert(1, format!("| {separator} |"));
    }
    lines.join("\n")
}

/// Plain text: links as `text (url)`, everything else as written.
struct Plain;

impl Dialect for Plain {
    fn text(&self, text: &str) -> String {
        text.to_owned()
    }
    fn code(&self, code: &str) -> String {
        format!("`{code}`")
    }
    fn fence(&self, lang: &str, body: &str) -> String {
        format!("```{lang}\n{body}\n```")
    }
    fn bold(&self, inner: String) -> String {
        format!("**{inner}**")
    }
    fn italic(&self, inner: String) -> String {
        format!("*{inner}*")
    }
    fn strike(&self, inner: String) -> String {
        format!("~~{inner}~~")
    }
    fn spoiler(&self, inner: String) -> String {
        format!("||{inner}||")
    }
    fn link(&self, label: String, url: &str) -> String {
        if label == url {
            label
        } else {
            format!("{label} ({url})")
        }
    }
    fn heading(&self, level: usize, inner: String) -> String {
        format!("{} {inner}", "#".repeat(level))
    }
    fn table(&self, rows: &[Vec<String>]) -> String {
        raw_table(rows)
    }
}

/// `WhatsApp` (and Signal-style) text: `*b*`, `_i_`, `~s~`, headings in
/// bold, links as `text (url)`.
struct Whatsapp;

impl Dialect for Whatsapp {
    fn text(&self, text: &str) -> String {
        text.to_owned()
    }
    fn code(&self, code: &str) -> String {
        format!("`{code}`")
    }
    fn fence(&self, lang: &str, body: &str) -> String {
        format!("```{lang}\n{body}\n```")
    }
    fn bold(&self, inner: String) -> String {
        format!("*{inner}*")
    }
    fn italic(&self, inner: String) -> String {
        format!("_{inner}_")
    }
    fn strike(&self, inner: String) -> String {
        format!("~{inner}~")
    }
    fn spoiler(&self, inner: String) -> String {
        inner
    }
    fn link(&self, label: String, url: &str) -> String {
        Plain.link(label, url)
    }
    fn heading(&self, _level: usize, inner: String) -> String {
        format!("*{inner}*")
    }
    fn table(&self, rows: &[Vec<String>]) -> String {
        format!("```\n{}\n```", raw_table(rows))
    }
}

/// Slack `mrkdwn`: `&`, `<`, `>` escaped, `<url|text>` links.
struct Mrkdwn;

impl Dialect for Mrkdwn {
    fn text(&self, text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
    fn code(&self, code: &str) -> String {
        format!("`{}`", self.text(code))
    }
    fn fence(&self, _lang: &str, body: &str) -> String {
        format!("```\n{}\n```", self.text(body))
    }
    fn bold(&self, inner: String) -> String {
        format!("*{inner}*")
    }
    fn italic(&self, inner: String) -> String {
        format!("_{inner}_")
    }
    fn strike(&self, inner: String) -> String {
        format!("~{inner}~")
    }
    fn spoiler(&self, inner: String) -> String {
        inner
    }
    fn link(&self, label: String, url: &str) -> String {
        format!("<{url}|{label}>")
    }
    fn heading(&self, _level: usize, inner: String) -> String {
        format!("*{inner}*")
    }
    fn table(&self, rows: &[Vec<String>]) -> String {
        format!("```\n{}\n```", self.text(&raw_table(rows)))
    }
}

/// Telegram `MarkdownV2`.
struct TelegramV2;

const V2_SPECIAL: &str = "_*[]()~`>#+-=|{}.!\\";

fn v2_escape(text: &str, special: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if special.contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

impl Dialect for TelegramV2 {
    fn text(&self, text: &str) -> String {
        v2_escape(text, V2_SPECIAL)
    }
    fn code(&self, code: &str) -> String {
        format!("`{}`", v2_escape(code, "`\\"))
    }
    fn fence(&self, lang: &str, body: &str) -> String {
        format!("```{lang}\n{}\n```", v2_escape(body, "`\\"))
    }
    fn bold(&self, inner: String) -> String {
        format!("*{inner}*")
    }
    fn italic(&self, inner: String) -> String {
        format!("_{inner}_")
    }
    fn strike(&self, inner: String) -> String {
        format!("~{inner}~")
    }
    fn spoiler(&self, inner: String) -> String {
        format!("||{inner}||")
    }
    fn link(&self, label: String, url: &str) -> String {
        format!("[{label}]({})", v2_escape(url, ")\\"))
    }
    fn heading(&self, _level: usize, inner: String) -> String {
        format!("*{inner}*")
    }
    fn table(&self, rows: &[Vec<String>]) -> String {
        let (max_rows, max_columns) = SMALL_TABLE;
        let columns = rows.first().map_or(0, Vec::len);
        if rows.len() > max_rows + 1 || columns > max_columns {
            return self.fence("", &raw_table(rows));
        }
        let header = rows.first().cloned().unwrap_or_default();
        let groups: Vec<String> = rows
            .iter()
            .skip(1)
            .map(|row| {
                let cells: Vec<String> = row
                    .iter()
                    .enumerate()
                    .map(|(i, cell)| {
                        let label = header.get(i).map_or("", String::as_str);
                        format!("*{}:* {}", self.text(label), self.pieces(&inline(cell)))
                    })
                    .collect();
                format!("• {}", cells.join("\n  "))
            })
            .collect();
        groups.join("\n")
    }
}

/// Matrix HTML (`<br>` line breaks, real headings and tables).
struct Html;

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

impl Dialect for Html {
    fn text(&self, text: &str) -> String {
        html_escape(text)
    }
    fn code(&self, code: &str) -> String {
        format!("<code>{}</code>", html_escape(code))
    }
    fn fence(&self, lang: &str, body: &str) -> String {
        if lang.is_empty() {
            format!("<pre><code>{}</code></pre>", html_escape(body))
        } else {
            format!(
                "<pre><code class=\"language-{}\">{}</code></pre>",
                html_escape(lang),
                html_escape(body)
            )
        }
    }
    fn bold(&self, inner: String) -> String {
        format!("<b>{inner}</b>")
    }
    fn italic(&self, inner: String) -> String {
        format!("<i>{inner}</i>")
    }
    fn strike(&self, inner: String) -> String {
        format!("<s>{inner}</s>")
    }
    fn spoiler(&self, inner: String) -> String {
        format!("<span data-mx-spoiler>{inner}</span>")
    }
    fn link(&self, label: String, url: &str) -> String {
        format!("<a href=\"{}\">{label}</a>", html_escape(url))
    }
    fn heading(&self, level: usize, inner: String) -> String {
        format!("<h{level}>{inner}</h{level}>")
    }
    fn table(&self, rows: &[Vec<String>]) -> String {
        let rows: Vec<String> = rows
            .iter()
            .enumerate()
            .map(|(i, row)| {
                let tag = if i == 0 { "th" } else { "td" };
                let cells: Vec<String> = row
                    .iter()
                    .map(|cell| format!("<{tag}>{}</{tag}>", self.pieces(&inline(cell))))
                    .collect();
                let cells = cells.concat();
                format!("<tr>{cells}</tr>")
            })
            .collect();
        format!("<table>{}</table>", rows.concat())
    }
    fn line_break(&self) -> &'static str {
        "<br>"
    }
}

/// What a part of a reply is sent as on the platform `descriptor`
/// describes; `plain` is the resend after a `formatting` refusal.
#[must_use]
pub fn render(descriptor: &Descriptor, text: &str, plain: bool) -> String {
    if plain {
        return Plain.render(text);
    }
    match (descriptor.markup, descriptor.platform.as_str()) {
        (Markup::Plain, _) => Plain.render(text),
        (Markup::Html, _) => Html.render(text),
        (Markup::Platform, "telegram") => TelegramV2.render(text),
        (Markup::Platform, "slack") => Mrkdwn.render(text),
        (Markup::Platform, "whatsapp") => Whatsapp.render(text),
        // Discord renders Markdown; Signal's adapter turns it into style
        // ranges; another platform's dialect is its adapter's business.
        (Markup::Markdown | Markup::Platform, _) => text.to_owned(),
    }
}

/// `text` rendered in `markup` for `platform` (an adapter's own fallback
/// dialect, such as Telegram `MarkdownV2` after a refused rich message).
#[must_use]
pub fn render_in(markup: Markup, platform: &str, text: &str) -> String {
    let descriptor = Descriptor {
        markup,
        ..Descriptor::new(platform, 1)
    };
    render(&descriptor, text, false)
}

/// `message` with its Markdown rendered for the platform `descriptor`
/// describes.
#[must_use]
pub fn rendered_with(descriptor: &Descriptor, message: &Outgoing) -> Outgoing {
    Outgoing {
        text: render(descriptor, &message.text, message.plain),
        ..message.clone()
    }
}

/// [`rendered_with`] for `adapter`'s descriptor.
#[must_use]
pub fn rendered(adapter: &dyn Adapter, message: &Outgoing) -> Outgoing {
    rendered_with(&adapter.describe(), message)
}

/// The visible text of HTML (Matrix's plain `body` next to its
/// `formatted_body`): tags dropped, line breaks kept, entities decoded.
#[must_use]
pub fn html_to_text(html: &str) -> String {
    let html = html.replace("<br>", "\n");
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

/// A Signal text style range, in UTF-16 code units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Style {
    /// Start offset.
    pub start: usize,
    /// Length.
    pub length: usize,
    /// `BOLD`, `ITALIC`, `MONOSPACE`, `STRIKETHROUGH` or `SPOILER`.
    pub kind: &'static str,
}

impl Style {
    /// signal-cli's `start:length:STYLE`.
    #[must_use]
    pub fn spec(&self) -> String {
        format!("{}:{}:{}", self.start, self.length, self.kind)
    }
}

/// The plain text of `text` and its native Signal style ranges (links as
/// `text (url)`, headings bold, fences monospace).
#[must_use]
pub fn signal_styles(text: &str) -> (String, Vec<Style>) {
    let mut styled = Styled::default();
    for (i, block) in blocks(text).iter().enumerate() {
        if i > 0 {
            styled.push("\n");
        }
        match block {
            Block::Line(p) => styled.pieces(p),
            Block::Heading(_, p) => styled.wrap("BOLD", |s| s.pieces(p)),
            Block::Fence(_, body) => styled.wrap("MONOSPACE", |s| s.push(body)),
            Block::Table(rows) => styled.wrap("MONOSPACE", |s| s.push(&raw_table(rows))),
        }
    }
    (styled.text, styled.styles)
}

#[derive(Default)]
struct Styled {
    text: String,
    units: usize,
    styles: Vec<Style>,
}

impl Styled {
    fn push(&mut self, text: &str) {
        self.units += text.encode_utf16().count();
        self.text.push_str(text);
    }

    fn wrap(&mut self, kind: &'static str, inner: impl FnOnce(&mut Self)) {
        let start = self.units;
        inner(self);
        if self.units > start {
            self.styles.push(Style {
                start,
                length: self.units - start,
                kind,
            });
        }
    }

    fn pieces(&mut self, pieces: &[Inline]) {
        for piece in pieces {
            match piece {
                Inline::Text(t) => self.push(t),
                Inline::Code(c) => self.wrap("MONOSPACE", |s| s.push(c)),
                Inline::Bold(p) => self.wrap("BOLD", |s| s.pieces(p)),
                Inline::Italic(p) => self.wrap("ITALIC", |s| s.pieces(p)),
                Inline::Strike(p) => self.wrap("STRIKETHROUGH", |s| s.pieces(p)),
                Inline::Spoiler(p) => self.wrap("SPOILER", |s| s.pieces(p)),
                Inline::Link(p, url) => {
                    self.pieces(p);
                    self.push(&format!(" ({url})"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
