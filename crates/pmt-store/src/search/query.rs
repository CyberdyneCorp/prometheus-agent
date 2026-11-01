//! Search query sanitisation (spec: session-storage, Search query
//! sanitisation): user text becomes an FTS5 query that cannot be a syntax
//! error. Balanced `"phrases"` are kept, an unbalanced quote and every
//! index-syntax character become spaces, leading wildcards and leading or
//! trailing boolean operators are dropped, and every term is quoted (so
//! `chat-send` searches the phrase `"chat-send"`); a trailing `*` keeps
//! prefix search.

/// Longest query accepted, in characters.
pub const MAX_QUERY_CHARS: usize = 2048;

/// One element of a sanitised query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Token {
    /// A quoted phrase from the input.
    Phrase(String),
    /// A word; `prefix` when it ended with `*`.
    Term { text: String, prefix: bool },
    /// `AND`, `OR` or `NOT` between two operands.
    Op(&'static str),
}

/// A sanitised query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Query {
    pub tokens: Vec<Token>,
}

impl Query {
    /// Sanitises `raw`.
    pub(crate) fn parse(raw: &str) -> Self {
        let capped: String = raw.chars().take(MAX_QUERY_CHARS).collect();
        let mut tokens = Vec::new();
        for (text, quoted) in split_phrases(&capped) {
            if quoted {
                let phrase = text.split_whitespace().collect::<Vec<_>>().join(" ");
                if !phrase.is_empty() {
                    tokens.push(Token::Phrase(phrase));
                }
            } else {
                tokens.extend(words(&text));
            }
        }
        Self {
            tokens: normalize_ops(tokens),
        }
    }

    /// The searched texts (phrases and terms), operators left out.
    pub(crate) fn terms(&self) -> Vec<&str> {
        self.tokens
            .iter()
            .filter_map(|token| match token {
                Token::Phrase(text) | Token::Term { text, .. } => Some(text.as_str()),
                Token::Op(_) => None,
            })
            .collect()
    }

    /// Whether nothing searchable is left.
    pub(crate) fn is_empty(&self) -> bool {
        self.terms().is_empty()
    }

    /// The FTS5 query text; `prefixes` false drops `*` (the trigram index
    /// has no prefix queries).
    pub(crate) fn render(&self, prefixes: bool) -> String {
        let parts: Vec<String> = self
            .tokens
            .iter()
            .map(|token| match token {
                Token::Term { text, prefix } if *prefix && prefixes => format!("\"{text}\"*"),
                Token::Phrase(text) | Token::Term { text, .. } => format!("\"{text}\""),
                Token::Op(op) => (*op).to_owned(),
            })
            .collect();
        parts.join(" ")
    }
}

/// `(text, quoted)` runs: text between balanced quotes is quoted; an
/// unbalanced last quote becomes a space.
fn split_phrases(text: &str) -> Vec<(String, bool)> {
    let quotes = text.matches('"').count();
    let usable = quotes - quotes % 2;
    let mut runs = Vec::new();
    let mut current = String::new();
    let mut seen = 0;
    let mut inside = false;
    for c in text.chars() {
        if c != '"' {
            current.push(c);
            continue;
        }
        seen += 1;
        if seen > usable {
            current.push(' ');
            continue;
        }
        runs.push((std::mem::take(&mut current), inside));
        inside = !inside;
    }
    runs.push((current, inside));
    runs
}

/// Characters kept in unquoted text; everything else is index syntax (or
/// punctuation) and becomes a space.
fn kept(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '*' | '.' | '-' | '_')
}

fn words(text: &str) -> Vec<Token> {
    let cleaned: String = text
        .chars()
        .map(|c| if kept(c) { c } else { ' ' })
        .collect();
    let mut tokens = Vec::new();
    for word in cleaned.split_whitespace() {
        if let Some(op) = operator(word) {
            tokens.push(Token::Op(op));
            continue;
        }
        let prefix = word.ends_with('*');
        let pieces: Vec<&str> = word.split('*').filter(|p| !p.is_empty()).collect();
        let last = pieces.len().saturating_sub(1);
        for (i, piece) in pieces.into_iter().enumerate() {
            tokens.push(Token::Term {
                text: piece.to_owned(),
                prefix: prefix && i == last,
            });
        }
    }
    tokens
}

fn operator(word: &str) -> Option<&'static str> {
    match word {
        "AND" => Some("AND"),
        "OR" => Some("OR"),
        "NOT" => Some("NOT"),
        _ => None,
    }
}

/// Operators only between operands: leading and trailing ones are
/// dropped, and of consecutive ones the last is kept.
fn normalize_ops(tokens: Vec<Token>) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    for token in tokens {
        let is_op = matches!(token, Token::Op(_));
        match out.last() {
            None if is_op => {}
            Some(Token::Op(_)) if is_op => {
                out.pop();
                out.push(token);
            }
            _ => out.push(token),
        }
    }
    while matches!(out.last(), Some(Token::Op(_))) {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{MAX_QUERY_CHARS, Query};

    fn render(raw: &str) -> String {
        Query::parse(raw).render(true)
    }

    #[test]
    fn hyphenated_term_becomes_a_phrase() {
        assert_eq!(render("chat-send"), "\"chat-send\"");
        assert_eq!(render("config.toml my_var"), "\"config.toml\" \"my_var\"");
    }

    #[test]
    fn balanced_phrases_survive_and_unbalanced_quotes_go() {
        assert_eq!(render("\"exact words\" other"), "\"exact words\" \"other\"");
        assert_eq!(render("say \"hello"), "\"say\" \"hello\"");
        assert_eq!(render("\"a\" \"b"), "\"a\" \"b\"");
    }

    #[test]
    fn syntax_characters_wildcards_and_dangling_operators_are_neutralised() {
        assert_eq!(render("NOT (secret OR"), "\"secret\"");
        assert_eq!(render("*foo bar*"), "\"foo\" \"bar\"*");
        assert_eq!(render("a AND OR b"), "\"a\" OR \"b\"");
        assert_eq!(
            render("col:value ^start {x} [y]"),
            "\"col\" \"value\" \"start\" \"x\" \"y\""
        );
        assert_eq!(render("NEAR(a b)"), "\"NEAR\" \"a\" \"b\"");
        assert!(Query::parse("\"\" ** AND ()").is_empty());
    }

    #[test]
    fn long_queries_are_capped() {
        let long = "x".repeat(MAX_QUERY_CHARS + 500);
        let terms = Query::parse(&long);
        assert_eq!(terms.terms()[0].chars().count(), MAX_QUERY_CHARS);
    }
}
