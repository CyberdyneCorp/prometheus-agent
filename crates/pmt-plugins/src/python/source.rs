//! A light reading of Python source for the static compatibility audit:
//! comments and string contents blanked out (so only code is matched),
//! the function each line belongs to, and the names each function calls.
//!
//! This is a heuristic reader, not a parser: it follows indentation and
//! bracket depth, which is enough to place a call inside its `def` and to
//! tell module-level code from function bodies.

use std::collections::BTreeSet;

/// One `def`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Def {
    /// The function name.
    pub(super) name: String,
    /// The enclosing `def`, by index.
    pub(super) parent: Option<usize>,
    /// The first parameter's name.
    pub(super) first_param: Option<String>,
    /// Names it calls (`f(...)`, `obj.f(...)`).
    pub(super) calls: BTreeSet<String>,
}

/// One read source file.
#[derive(Debug, Clone)]
pub(super) struct Source {
    /// The lines as written.
    pub(super) raw: Vec<String>,
    /// The lines with comments and string contents replaced by spaces
    /// (quotes kept), same length as `raw`.
    pub(super) code: Vec<String>,
    /// The innermost `def` of each line (`None`: module level).
    pub(super) owner: Vec<Option<usize>>,
    /// Every `def`.
    pub(super) defs: Vec<Def>,
}

impl Source {
    /// Reads `text`.
    #[must_use]
    pub(super) fn read(text: &str) -> Self {
        let raw: Vec<String> = text.lines().map(str::to_owned).collect();
        let code = blank(text);
        let mut source = Self {
            owner: vec![None; raw.len()],
            raw,
            code,
            defs: Vec::new(),
        };
        source.place();
        source
    }

    /// Assigns each line its `def` and collects the calls of each.
    fn place(&mut self) {
        let mut stack: Vec<(usize, usize)> = Vec::new();
        let mut depth = 0i32;
        let mut last_owner = None;
        for at in 0..self.code.len() {
            let line = self.code[at].clone();
            let continuation = depth > 0;
            depth = (depth + bracket_delta(&line)).max(0);
            if continuation || line.trim().is_empty() {
                self.owner[at] = last_owner;
                self.note_calls(at, last_owner);
                continue;
            }
            let indent = line.len() - line.trim_start().len();
            while stack.last().is_some_and(|(_, i)| *i >= indent) {
                stack.pop();
            }
            let owner = stack.last().map(|(d, _)| *d);
            if let Some(def) = def_of(&line, owner) {
                self.defs.push(def);
                let index = self.defs.len() - 1;
                stack.push((index, indent));
                last_owner = Some(index);
                self.owner[at] = Some(index);
                continue;
            }
            last_owner = owner;
            self.owner[at] = owner;
            self.note_calls(at, owner);
        }
    }

    fn note_calls(&mut self, at: usize, owner: Option<usize>) {
        if let Some(def) = owner {
            let calls = called_names(&self.code[at]);
            self.defs[def].calls.extend(calls);
        }
    }
}

/// `def name(first, ...)` on a code line.
fn def_of(line: &str, parent: Option<usize>) -> Option<Def> {
    let text = line.trim_start();
    let text = text.strip_prefix("async ").map_or(text, str::trim_start);
    let rest = text.strip_prefix("def ")?.trim_start();
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() {
        return None;
    }
    let params = rest[name.len()..].trim_start().strip_prefix('(')?;
    let first: String = params
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    let first_param = match first.as_str() {
        "" => None,
        "self" | "cls" => params
            .split(',')
            .nth(1)
            .map(|p| {
                p.trim()
                    .split([':', '=', ')'])
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_owned()
            })
            .filter(|p| !p.is_empty()),
        _ => Some(first),
    };
    Some(Def {
        name,
        parent,
        first_param,
        calls: BTreeSet::new(),
    })
}

/// Net bracket depth change of a code line.
fn bracket_delta(line: &str) -> i32 {
    line.chars()
        .map(|c| match c {
            '(' | '[' | '{' => 1,
            ')' | ']' | '}' => -1,
            _ => 0,
        })
        .sum()
}

/// Names followed by `(` on a code line.
#[must_use]
pub(super) fn called_names(line: &str) -> Vec<String> {
    let mut names = Vec::new();
    let bytes = line.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        if is_ident_start(bytes[at]) && (at == 0 || !is_ident(bytes[at - 1])) {
            let start = at;
            while at < bytes.len() && is_ident(bytes[at]) {
                at += 1;
            }
            let after = line[at..].trim_start();
            if after.starts_with('(') {
                names.push(line[start..at].to_owned());
            }
            continue;
        }
        at += 1;
    }
    names
}

fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Scanner state of [`blank`].
#[derive(Clone, Copy)]
enum State {
    Code,
    Comment,
    Str { quote: char, triple: bool },
}

/// `text` split into lines with comments and string contents blanked.
fn blank(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut state = State::Code;
    let mut at = 0;
    while at < chars.len() {
        let c = chars[at];
        let (next, consumed, emitted) = step(state, &chars, at);
        state = next;
        if c == '\n' {
            out.push('\n');
        } else {
            out.push_str(&emitted);
        }
        at += consumed;
    }
    out.lines().map(str::to_owned).collect()
}

/// One scanner step: the next state, the characters consumed and what
/// stands for them in the blanked text.
fn step(state: State, chars: &[char], at: usize) -> (State, usize, String) {
    let c = chars[at];
    let triple_at = |q: char| {
        chars
            .get(at..at + 3)
            .is_some_and(|s| s.iter().all(|x| *x == q))
    };
    match state {
        State::Comment if c == '\n' => (State::Code, 1, "\n".into()),
        State::Comment => (State::Comment, 1, " ".into()),
        State::Code if c == '#' => (State::Comment, 1, " ".into()),
        State::Code if c == '"' || c == '\'' => {
            let triple = triple_at(c);
            let width = if triple { 3 } else { 1 };
            (
                State::Str { quote: c, triple },
                width,
                c.to_string().repeat(width),
            )
        }
        State::Code => (State::Code, 1, c.to_string()),
        State::Str { .. } if c == '\\' => (state, 2.min(chars.len() - at), "  ".into()),
        State::Str {
            quote,
            triple: true,
        } if c == quote && triple_at(quote) => (State::Code, 3, quote.to_string().repeat(3)),
        State::Str {
            quote,
            triple: false,
        } if c == quote => (State::Code, 1, quote.to_string()),
        State::Str { triple: false, .. } if c == '\n' => (State::Code, 1, "\n".into()),
        State::Str { .. } => (state, 1, if c == '\n' { "\n".into() } else { " ".into() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_and_strings_are_not_code() {
        let source = Source::read("x = 'ctx.register_platform()'  # ctx.register_platform()\n");
        assert!(!source.code[0].contains("register_platform"));
        assert!(source.raw[0].contains("register_platform"));
    }

    #[test]
    fn calls_belong_to_their_function() {
        let text = "def register(ctx):\n    helper(ctx)\n    def handler(args):\n        ctx.x()\n    ctx.register_tool(handler=handler)\n\ndef helper(c):\n    c.y()\nmain()\n";
        let source = Source::read(text);
        let register = &source.defs[0];
        assert_eq!(register.first_param.as_deref(), Some("ctx"));
        assert!(register.calls.contains("helper"));
        assert!(!register.calls.contains("handler"));
        assert_eq!(source.owner[3], Some(1));
        assert_eq!(source.owner[4], Some(0));
        assert_eq!(source.owner[8], None);
    }

    #[test]
    fn a_multi_line_call_stays_in_its_function() {
        let text = "def register(ctx):\n    ctx.register_tool(\nname='a',\n    )\nafter = 1\n";
        let source = Source::read(text);
        assert_eq!(source.owner[2], Some(0));
        assert_eq!(source.owner[4], None);
    }
}
