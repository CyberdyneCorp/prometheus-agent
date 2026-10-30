//! What a command line does to the foreground (spec: execution-environments,
//! Foreground cap, promotion and server guard).
//!
//! A lone trailing `&` means "run in the background". A foreground command
//! that detaches a process (`nohup`, `disown`, `setsid`, an inline `&`) or
//! starts a long-lived server/watcher is refused with guidance, because it
//! would either hang the call until its timeout or leave an untracked
//! process behind. `--help`/`--version` invocations are never servers.
//!
//! The lexer understands quotes, comments, redirections such as `2>&1`
//! and `&>`, and PowerShell's `&` call operator; it is a classifier, not a
//! shell parser.

/// How to run a command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Run it in the foreground as given.
    Foreground,
    /// Run `command` (the line without its trailing `&`) in the background.
    Background {
        /// The command line to run.
        command: String,
    },
    /// Refuse with this message.
    Refuse(String),
}

/// The first sentence of the server-guard refusal.
pub const SERVER_REFUSAL: &str =
    "This foreground command appears to start a long-lived server/watch process.";

const RESEND: &str =
    "Resend it with background=true, then use the process tool to poll its output or kill it.";

/// Classifies a foreground command line.
#[must_use]
pub fn classify(command: &str) -> Plan {
    let tokens = lex(command);
    let last_op = tokens
        .iter()
        .rposition(|t| !matches!(t, Token::Op(_, "\n")));
    if let Some(index) = last_op
        && let Token::Op(at, "&") = tokens[index]
    {
        return Plan::Background {
            command: command[..at].trim_end().to_owned(),
        };
    }
    if tokens.iter().any(|t| matches!(t, Token::Op(_, "&"))) {
        return Plan::Refuse(detach_refusal("&"));
    }
    for segment in segments(&tokens) {
        let words = command_words(&segment);
        if let Some(first) = words.first()
            && let Some(detach) = ["nohup", "disown", "setsid"].iter().find(|d| *d == first)
        {
            return Plan::Refuse(detach_refusal(detach));
        }
        if is_server(&words) {
            return Plan::Refuse(format!("{SERVER_REFUSAL} {RESEND}"));
        }
    }
    Plan::Foreground
}

fn detach_refusal(how: &str) -> String {
    format!(
        "This foreground command detaches a process with '{how}', which would outlive the call untracked. {RESEND}"
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    /// A control operator and its byte offset.
    Op(usize, &'static str),
}

const OPERATORS: &[&str] = &["&&", "||", "|&", ";;", "&", "|", ";", "\n", "(", ")"];

/// Splits `line` into words and control operators.
fn lex(line: &str) -> Vec<Token> {
    let mut lexer = Lexer::default();
    let mut chars = line.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if lexer.quote.is_some() {
            lexer.quoted(c, &mut chars);
        } else {
            lexer.unquoted(&line[at..], at, c, &mut chars);
        }
    }
    lexer.finish_word();
    lexer.tokens
}

type Chars<'a> = std::iter::Peekable<std::str::CharIndices<'a>>;

#[derive(Default)]
struct Lexer {
    tokens: Vec<Token>,
    word: String,
    quote: Option<char>,
}

impl Lexer {
    /// One character outside quotes; `rest` starts with it, at byte `at`.
    fn unquoted(&mut self, rest: &str, at: usize, c: char, chars: &mut Chars<'_>) {
        match c {
            '\\' => {
                self.word.push(c);
                if let Some((_, next)) = chars.next() {
                    self.word.push(next);
                }
            }
            '\'' | '"' => {
                self.quote = Some(c);
                self.word.push(c);
            }
            '#' if self.word.is_empty() => while chars.next_if(|(_, c)| *c != '\n').is_some() {},
            // `>&`, `<&` and `&>` are redirections, not control operators.
            '&' if self.word.ends_with(['>', '<']) || rest.starts_with("&>") => self.word.push(c),
            // In command position (nothing before it in this command) `&` is
            // PowerShell's call operator, `& 'C:\x\tool.exe' args`; POSIX
            // shells reject it there, so this reading is safe for both.
            '&' if self.at_command_start() && !rest.starts_with("&&") => self.word.push(c),
            c if c.is_whitespace() && c != '\n' => self.finish_word(),
            _ => match OPERATORS.iter().find(|op| rest.starts_with(**op)) {
                Some(op) => {
                    self.finish_word();
                    self.tokens.push(Token::Op(at, op));
                    for _ in 1..op.len() {
                        chars.next();
                    }
                }
                None => self.word.push(c),
            },
        }
    }

    fn quoted(&mut self, c: char, chars: &mut Chars<'_>) {
        self.word.push(c);
        if c == '\\' && self.quote == Some('"') {
            if let Some((_, next)) = chars.next() {
                self.word.push(next);
            }
        } else if Some(c) == self.quote {
            self.quote = None;
        }
    }

    fn at_command_start(&self) -> bool {
        self.word.is_empty() && !matches!(self.tokens.last(), Some(Token::Word(_)))
    }

    fn finish_word(&mut self) {
        if !self.word.is_empty() {
            self.tokens
                .push(Token::Word(std::mem::take(&mut self.word)));
        }
    }
}

/// The words of every simple command, split at control operators.
fn segments(tokens: &[Token]) -> Vec<Vec<&str>> {
    let mut all = vec![Vec::new()];
    for token in tokens {
        match token {
            Token::Word(word) => all.last_mut().expect("never empty").push(word.as_str()),
            Token::Op(..) => all.push(Vec::new()),
        }
    }
    all.retain(|s| !s.is_empty());
    all
}

/// The command and its arguments: leading `NAME=value` assignments and
/// transparent wrappers are skipped, quotes are dropped.
fn command_words(segment: &[&str]) -> Vec<String> {
    // `&` is PowerShell's call operator (see the lexer).
    const WRAPPERS: &[&str] = &["sudo", "env", "exec", "time", "command", "npx", "bunx", "&"];
    let mut words: Vec<String> = segment.iter().map(|w| w.replace(['\'', '"'], "")).collect();
    while let Some(first) = words.first() {
        let assignment = first
            .find('=')
            .is_some_and(|eq| eq > 0 && !first.starts_with('-'));
        if assignment || WRAPPERS.contains(&first.as_str()) {
            words.remove(0);
        } else {
            break;
        }
    }
    words
}

fn is_server(words: &[String]) -> bool {
    let has = |w: &str| words.iter().any(|x| x == w);
    if ["--help", "-h", "--version", "-V"].iter().any(|w| has(w)) {
        return false;
    }
    let word = |i: usize| words.get(i).map_or("", String::as_str);
    let task = |w: &str| matches!(w, "dev" | "start" | "serve" | "watch");
    match word(0) {
        "npm" | "pnpm" | "yarn" | "bun" => task(word(1)) || (word(1) == "run" && task(word(2))),
        "docker" => word(1) == "compose" && word(2) == "up" && !has("-d") && !has("--detach"),
        "docker-compose" => word(1) == "up" && !has("-d") && !has("--detach"),
        "next" => word(1) == "dev",
        "vite" => !matches!(word(1), "build" | "optimize"),
        "nodemon" | "uvicorn" | "gunicorn" => true,
        "cargo" => word(1) == "watch",
        python if is_python(python) => words
            .windows(2)
            .any(|w| w[0] == "-m" && w[1] == "http.server"),
        _ => false,
    }
}

fn is_python(word: &str) -> bool {
    let name = word.rsplit('/').next().unwrap_or(word);
    name == "python"
        || name
            .strip_prefix("python3")
            .is_some_and(|v| v.is_empty() || v.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::{Plan, SERVER_REFUSAL, classify};

    fn refused(command: &str) -> String {
        match classify(command) {
            Plan::Refuse(message) => message,
            other => panic!("{command}: expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn dev_servers_in_foreground_are_refused() {
        for command in [
            "npm run dev",
            "pnpm dev",
            "yarn start",
            "bun run watch",
            "docker compose up",
            "next dev",
            "npx vite",
            "nodemon app.js",
            "PORT=3000 uvicorn app:main",
            "cd web && npm run serve",
            "cargo watch -x test",
            "python3 -m http.server 8000",
        ] {
            assert!(refused(command).starts_with(SERVER_REFUSAL), "{command}");
        }
    }

    #[test]
    fn help_version_and_one_shot_commands_run() {
        for command in [
            "npm run dev --help",
            "vite --version",
            "uvicorn -V",
            "npm test",
            "npm run build",
            "vite build",
            "docker compose up -d",
            "cargo test",
            "python -m pytest",
            "echo 'npm run dev'",
            "ls 2>&1 | grep x",
            "make &> log.txt",
            "a && b || c",
        ] {
            assert_eq!(classify(command), Plan::Foreground, "{command}");
        }
    }

    #[test]
    fn trailing_ampersand_means_background() {
        assert_eq!(
            classify("python server.py > log 2>&1 &"),
            Plan::Background {
                command: "python server.py > log 2>&1".to_owned()
            }
        );
        assert_eq!(
            classify("npm run dev &\n"),
            Plan::Background {
                command: "npm run dev".to_owned()
            }
        );
    }

    #[test]
    fn detaching_in_foreground_is_refused() {
        for (command, how) in [
            ("nohup ./job.sh", "nohup"),
            ("./job.sh & disown", "&"),
            ("setsid ./job.sh", "setsid"),
            ("sleep 100 & echo started", "&"),
        ] {
            assert!(refused(command).contains(&format!("'{how}'")), "{command}");
        }
        assert_eq!(classify("grep nohup notes.txt"), Plan::Foreground);
        assert_eq!(classify("echo 'a & b'"), Plan::Foreground);
    }

    #[test]
    fn powershell_call_operator_is_not_a_detach() {
        for command in [
            "& 'C:\\Tools\\cmd.exe' /c exit 3",
            "& ([Environment]::SystemDirectory + '\\cmd.exe') /c exit 3",
            "Get-Date; & $tool --check",
            "Get-Item x | &{ process { $_ } }",
        ] {
            assert_eq!(classify(command), Plan::Foreground, "{command}");
        }
        assert!(refused("& npm run dev").starts_with(SERVER_REFUSAL));
        assert!(refused("& ./job.ps1 & Write-Output started").contains("'&'"));
    }
}
