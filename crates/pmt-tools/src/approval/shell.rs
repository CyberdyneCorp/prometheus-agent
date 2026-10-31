//! A small reader of POSIX-shell command lines for the approval gate.
//!
//! It does not run or expand anything: it splits a command line into the
//! simple commands it contains (separated by `;`, `&&`, `||`, `|`, `&`,
//! newlines and parentheses), each with its words (quotes removed,
//! variables kept literally as `$NAME`) and redirections. Command
//! substitutions (`$(...)`, backticks) and the bodies of `sh -c` / `bash -c`
//! / `eval` are read too and their commands appended, so a hardline command
//! cannot hide inside them. Over-reading is harmless here: a false match
//! only asks or blocks, a missed one would run.

/// One simple command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Segment {
    /// Words with quoting removed.
    pub words: Vec<String>,
    /// Redirections, in order.
    pub redirects: Vec<Redirect>,
    /// Whether the previous command's output is piped into this one.
    pub piped: bool,
}

/// One redirection (`> file`, `2>> log`, `< input`...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    /// The operator without its file descriptor (`>`, `>>`, `<`...).
    pub op: String,
    /// The target word.
    pub target: String,
}

impl Redirect {
    /// Whether it writes to its target.
    #[must_use]
    pub fn writes(&self) -> bool {
        self.op.contains('>')
    }
}

/// The simple commands of a command line, nested ones included.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Script {
    /// Every simple command, outer ones first.
    pub segments: Vec<Segment>,
}

/// Why a command line could not be read (`approvals test` exits 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How deep `sh -c` / substitution bodies are followed.
const MAX_DEPTH: usize = 8;

/// Shells whose `-c` argument is itself a command line.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish", "busybox"];

/// Reads `text`.
///
/// # Errors
///
/// An unterminated quote, substitution or escape.
pub fn parse(text: &str) -> Result<Script, ParseError> {
    let mut script = Script::default();
    parse_into(text, 0, &mut script)?;
    Ok(script)
}

fn parse_into(text: &str, depth: usize, script: &mut Script) -> Result<(), ParseError> {
    if depth > MAX_DEPTH {
        return Err(ParseError("command nests too deeply".into()));
    }
    let mut lexer = Lexer::new(text);
    lexer.run()?;
    for segment in lexer.segments {
        let inner = segment.program().and_then(|(program, args)| {
            let shell = SHELLS.contains(&program);
            let body = args
                .iter()
                .position(|a| a == "-c")
                .and_then(|i| args.get(i + 1));
            match (shell, program) {
                (true, _) => body.cloned(),
                (false, "eval") => Some(args.join(" ")),
                _ => None,
            }
        });
        script.segments.push(segment);
        if let Some(body) = inner {
            parse_into(&body, depth + 1, script)?;
        }
    }
    for nested in lexer.nested {
        parse_into(&nested, depth + 1, script)?;
    }
    Ok(())
}

/// Leading words that run the next word as the program.
const WRAPPERS: &[&str] = &[
    "sudo", "doas", "env", "nohup", "exec", "time", "command", "builtin", "nice", "ionice",
    "timeout", "stdbuf", "xargs", "then", "do", "else", "elif", "if", "while", "until", "!", "{",
];

/// Wrapper options that take a separate value.
const VALUE_OPTIONS: &[&str] = &[
    "-u", "-g", "-h", "-p", "-C", "-D", "-r", "-t", "-U", "-n", "-c",
];

impl Segment {
    /// The program's base name (`/bin/rm` -> `rm`, `rm.exe` -> `rm`) and
    /// its arguments, after variable assignments and wrappers such as
    /// `sudo` or `env`.
    #[must_use]
    pub fn program(&self) -> Option<(&str, &[String])> {
        let (at, _) = self.locate();
        let at = at?;
        Some((basename(&self.words[at]), &self.words[at + 1..]))
    }

    /// Whether a privilege wrapper (`sudo`, `doas`) precedes the program.
    #[must_use]
    pub fn elevated(&self) -> bool {
        self.locate().1
    }

    /// The program's index and whether `sudo`/`doas` wraps it.
    fn locate(&self) -> (Option<usize>, bool) {
        let mut i = 0;
        let mut wrapper: Option<&str> = None;
        let mut elevated = false;
        while let Some(word) = self.words.get(i) {
            let name = basename(word);
            if is_assignment(word) {
                i += 1;
            } else if wrapper.is_some() && word.starts_with('-') {
                i += if VALUE_OPTIONS.contains(&word.as_str()) {
                    2
                } else {
                    1
                };
            } else if wrapper == Some("timeout") {
                // `timeout DURATION cmd`
                wrapper = None;
                i += 1;
            } else if WRAPPERS.contains(&name) {
                elevated |= matches!(name, "sudo" | "doas");
                wrapper = Some(name);
                i += 1;
            } else {
                return (Some(i), elevated);
            }
        }
        (None, elevated)
    }

    /// The words joined by single spaces (what allow and deny rules see).
    #[must_use]
    pub fn text(&self) -> String {
        self.words.join(" ")
    }
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with(|c: char| c.is_ascii_digit())
    })
}

/// `/usr/bin/rm` -> `rm`, `C:\x\rm.exe` -> `rm`.
#[must_use]
pub fn basename(word: &str) -> &str {
    let name = word.rsplit(['/', '\\']).next().unwrap_or(word);
    name.strip_suffix(".exe")
        .or_else(|| name.strip_suffix(".EXE"))
        .unwrap_or(name)
}

struct Lexer<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    segments: Vec<Segment>,
    nested: Vec<String>,
    current: Segment,
    word: Option<String>,
    pending_redirect: Option<String>,
}

impl<'a> Lexer<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            chars: text.chars().peekable(),
            segments: Vec::new(),
            nested: Vec::new(),
            current: Segment::default(),
            word: None,
            pending_redirect: None,
        }
    }

    fn run(&mut self) -> Result<(), ParseError> {
        while let Some(c) = self.chars.next() {
            match c {
                ' ' | '\t' => self.end_word(),
                '\n' | ';' | '(' | ')' => self.end_segment(false),
                '&' | '|' => self.operator(c),
                '>' | '<' => self.redirect(c),
                '#' if self.word.is_none() => self.skip_comment(),
                '\'' => self.single_quoted()?,
                '"' => self.double_quoted()?,
                '\\' => self.escaped(),
                '$' if self.chars.peek() == Some(&'(') => self.substitution()?,
                '`' => self.backticks()?,
                _ => self.push(c),
            }
        }
        self.end_word();
        if let Some(op) = &self.pending_redirect {
            return Err(ParseError(format!("redirection '{op}' has no target")));
        }
        self.end_segment(false);
        Ok(())
    }

    fn push(&mut self, c: char) {
        self.word.get_or_insert_with(String::new).push(c);
    }

    fn push_str(&mut self, s: &str) {
        self.word.get_or_insert_with(String::new).push_str(s);
    }

    fn end_word(&mut self) {
        let Some(word) = self.word.take() else {
            return;
        };
        match self.pending_redirect.take() {
            Some(op) => self.current.redirects.push(Redirect { op, target: word }),
            None => self.current.words.push(word),
        }
    }

    fn end_segment(&mut self, piped_next: bool) {
        self.end_word();
        let segment = std::mem::take(&mut self.current);
        if !segment.words.is_empty() || !segment.redirects.is_empty() {
            self.segments.push(segment);
        }
        self.current.piped = piped_next;
    }

    /// `&`, `&&`, `|`, `||`, `|&`, and `&>` / `&>>` redirections.
    fn operator(&mut self, c: char) {
        if c == '&' && self.chars.peek() == Some(&'>') {
            self.end_word();
            self.chars.next();
            let op = if self.chars.next_if_eq(&'>').is_some() {
                "&>>"
            } else {
                "&>"
            };
            self.pending_redirect = Some(op.to_owned());
            return;
        }
        let doubled = self.chars.next_if_eq(&c).is_some();
        let pipe_amp = c == '|' && !doubled && self.chars.next_if_eq(&'&').is_some();
        self.end_segment(c == '|' && (!doubled || pipe_amp));
    }

    /// `>`, `>>`, `>|`, `>&`, `<`, `<<`, `<<<`, `<&`, with an optional
    /// leading file descriptor (`2>`).
    fn redirect(&mut self, c: char) {
        let fd_prefix = self
            .word
            .as_deref()
            .is_some_and(|w| !w.is_empty() && w.chars().all(|d| d.is_ascii_digit()));
        if fd_prefix {
            self.word = None;
        } else {
            self.end_word();
        }
        let mut op = c.to_string();
        while let Some(next) = self.chars.next_if(|&n| n == c || n == '|' || n == '&') {
            op.push(next);
        }
        // `2>&1` duplicates a descriptor: no file target.
        if op.ends_with('&') && self.chars.peek().is_some_and(char::is_ascii_digit) {
            while self.chars.next_if(char::is_ascii_digit).is_some() {}
            return;
        }
        self.pending_redirect = Some(op);
    }

    fn skip_comment(&mut self) {
        while self.chars.next_if(|&c| c != '\n').is_some() {}
    }

    fn single_quoted(&mut self) -> Result<(), ParseError> {
        let word = self.word.get_or_insert_with(String::new);
        loop {
            match self.chars.next() {
                Some('\'') => return Ok(()),
                Some(c) => word.push(c),
                None => return Err(ParseError("unterminated single quote".into())),
            }
        }
    }

    fn double_quoted(&mut self) -> Result<(), ParseError> {
        self.word.get_or_insert_with(String::new);
        loop {
            match self.chars.next() {
                Some('"') => return Ok(()),
                Some('\\') => match self.chars.next() {
                    Some(c @ ('"' | '\\' | '$' | '`')) => self.push(c),
                    Some('\n') => {}
                    Some(c) => {
                        self.push('\\');
                        self.push(c);
                    }
                    None => return Err(ParseError("unterminated double quote".into())),
                },
                Some('$') if self.chars.peek() == Some(&'(') => self.substitution()?,
                Some('`') => self.backticks()?,
                Some(c) => self.push(c),
                None => return Err(ParseError("unterminated double quote".into())),
            }
        }
    }

    /// A backslash quotes the next character, except inside a Windows
    /// path (`C:\Windows`) and at the very end, where it is literal.
    fn escaped(&mut self) {
        let windows_path = self.word.as_deref().is_some_and(|w| {
            let b = w.as_bytes();
            b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
        });
        match self.chars.next() {
            Some('\n') => {}
            Some(c) if windows_path => {
                self.push('\\');
                self.push(c);
            }
            Some(c) => self.push(c),
            None => self.push('\\'),
        }
    }

    /// `$( ... )`: the body is kept in the word and read as commands.
    fn substitution(&mut self) -> Result<(), ParseError> {
        self.chars.next(); // '('
        let mut depth = 1;
        let mut body = String::new();
        let mut quote: Option<char> = None;
        loop {
            let c = self
                .chars
                .next()
                .ok_or_else(|| ParseError("unterminated command substitution".into()))?;
            match (quote, c) {
                (Some(q), c) if c == q => quote = None,
                (None, '\'' | '"') => quote = Some(c),
                (None, '(') => depth += 1,
                (None, ')') => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            body.push(c);
        }
        self.push_str(&format!("$({body})"));
        self.nested.push(body);
        Ok(())
    }

    fn backticks(&mut self) -> Result<(), ParseError> {
        let mut body = String::new();
        loop {
            match self.chars.next() {
                Some('`') => break,
                Some('\\') => {
                    if let Some(c) = self.chars.next() {
                        body.push(c);
                    }
                }
                Some(c) => body.push(c),
                None => return Err(ParseError("unterminated backtick substitution".into())),
            }
        }
        self.push_str(&format!("`{body}`"));
        self.nested.push(body);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Redirect, parse};

    fn programs(text: &str) -> Vec<String> {
        parse(text)
            .unwrap()
            .segments
            .iter()
            .filter_map(|s| s.program().map(|(p, _)| p.to_owned()))
            .collect()
    }

    #[test]
    fn splits_simple_commands_and_removes_quotes() {
        let script = parse(r#"cd 'a b' && echo "x $Y" | grep -v z; ls\ -l"#).unwrap();
        let words: Vec<Vec<String>> = script.segments.iter().map(|s| s.words.clone()).collect();
        assert_eq!(
            words,
            vec![
                vec!["cd".to_owned(), "a b".into()],
                vec!["echo".into(), "x $Y".into()],
                vec!["grep".into(), "-v".into(), "z".into()],
                vec!["ls -l".into()],
            ]
        );
        assert!(script.segments[2].piped && !script.segments[1].piped);
    }

    #[test]
    fn redirections_are_separated_from_words() {
        let script = parse("echo hi > /dev/sda 2>&1; cat < in 2>>err").unwrap();
        assert_eq!(script.segments[0].words, ["echo", "hi"]);
        assert_eq!(
            script.segments[0].redirects,
            [Redirect {
                op: ">".into(),
                target: "/dev/sda".into()
            }]
        );
        assert_eq!(script.segments[1].redirects.len(), 2);
        assert_eq!(script.segments[1].redirects[1].op, ">>");
    }

    #[test]
    fn wrappers_and_nested_commands_are_read() {
        assert_eq!(programs("sudo -u root FOO=1 /bin/rm -rf x"), ["rm"]);
        assert_eq!(programs("timeout 5 env A=b nohup kill -1"), ["kill"]);
        assert_eq!(programs("bash -c 'rm -rf /'"), ["bash", "rm"]);
        assert_eq!(
            programs("echo $(rm -rf /) `mkfs /dev/x`"),
            ["echo", "rm", "mkfs"]
        );
        assert_eq!(programs("(cd /; rm -rf *)"), ["cd", "rm"]);
        assert!(parse("sudo rm x").unwrap().segments[0].elevated());
        assert!(!parse("rm sudo").unwrap().segments[0].elevated());
    }

    #[test]
    fn unterminated_quotes_are_errors() {
        assert!(parse("echo 'oops").is_err());
        assert!(parse("echo \"oops").is_err());
        assert!(parse("echo $(oops").is_err());
        assert!(parse("echo >").is_err());
    }
}
