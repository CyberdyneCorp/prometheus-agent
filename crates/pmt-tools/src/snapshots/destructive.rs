//! Terminal commands that change files in ways a checkpoint should cover
//! (spec: execution-environments, Snapshot triggers, caps and per-run
//! toggle): `rm`, `rmdir`, `cp`, `mv`, `install`, `sed -i`, `truncate`,
//! `dd`, `shred`, output redirection `>`, `git reset`, `git clean`,
//! `git checkout` and `git restore`.

/// Programs that are destructive whatever their arguments.
const PROGRAMS: &[&str] = &[
    "rm", "rmdir", "cp", "mv", "install", "truncate", "dd", "shred",
];
/// `git` subcommands that discard work.
const GIT: &[&str] = &["reset", "clean", "checkout", "restore"];
/// Words that run the next word as the command.
const WRAPPERS: &[&str] = &["sudo", "env", "command", "nice", "nohup", "time", "exec"];

/// Whether `command` has a destructive shape.
#[must_use]
pub fn is_destructive(command: &str) -> bool {
    redirects_to_a_file(command) || segments(command).iter().any(|words| destructive(words))
}

/// The simple commands of a line (split on `;`, `&`, `|` and newlines),
/// as words with quotes removed.
fn segments(command: &str) -> Vec<Vec<String>> {
    let mut segments = vec![Vec::new()];
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let push = |word: &mut String, segments: &mut Vec<Vec<String>>| {
        if !word.is_empty() {
            segments
                .last_mut()
                .expect("one segment")
                .push(std::mem::take(word));
        }
    };
    for c in command.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (None, '\'' | '"') => quote = Some(c),
            (None, ';' | '&' | '|' | '\n' | '(' | ')') => {
                push(&mut word, &mut segments);
                segments.push(Vec::new());
            }
            (None, c) if c.is_whitespace() => push(&mut word, &mut segments),
            (_, c) => word.push(c),
        }
    }
    push(&mut word, &mut segments);
    segments.retain(|s| !s.is_empty());
    segments
}

fn destructive(words: &[String]) -> bool {
    let mut words = words
        .iter()
        .skip_while(|w| w.contains('=') && !w.starts_with('-'))
        .peekable();
    while words.peek().is_some_and(|w| WRAPPERS.contains(&program(w))) {
        words.next();
        while words
            .peek()
            .is_some_and(|w| w.starts_with('-') || w.contains('='))
        {
            words.next();
        }
    }
    let Some(first) = words.next() else {
        return false;
    };
    let rest: Vec<&String> = words.collect();
    match program(first) {
        name if PROGRAMS.contains(&name) => true,
        "sed" => rest
            .iter()
            .any(|w| w.starts_with("-i") || *w == "--in-place" || w.starts_with("--in-place=")),
        "git" => git_subcommand(&rest).is_some_and(|sub| GIT.contains(&sub)),
        _ => false,
    }
}

/// The subcommand of `git <args>`, past global options (`-C <dir>` and
/// `-c <key=value>` take a value).
fn git_subcommand<'a>(args: &[&'a String]) -> Option<&'a str> {
    let mut words = args.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "-C" | "-c" => {
                words.next();
            }
            option if option.starts_with('-') => {}
            sub => return Some(sub),
        }
    }
    None
}

/// The program name of a word (`/bin/rm` is `rm`).
fn program(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// An unquoted `>` or `>>` whose target is a file (not `&N`, not
/// `/dev/null`).
fn redirects_to_a_file(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    let mut quote: Option<char> = None;
    for (i, &c) in chars.iter().enumerate() {
        match quote {
            Some(q) if c == q => quote = None,
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == '>'
                && chars.get(i + 1) != Some(&'>')
                && redirect_target(&chars[i + 1..]).is_some_and(|t| t != "/dev/null") =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

/// The file a redirection writes to, from the text after its `>` (`None`
/// for a descriptor duplication such as `2>&1`).
fn redirect_target(after: &[char]) -> Option<String> {
    let rest = match after.first() {
        Some('|') => &after[1..],
        Some('&') => return None,
        _ => after,
    };
    let target: String = rest
        .iter()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| !c.is_whitespace() && !matches!(c, ';' | '&' | '|'))
        .collect();
    (!target.is_empty()).then_some(target)
}

#[cfg(test)]
mod tests {
    use super::is_destructive;

    #[test]
    fn destructive_shapes_are_recognised() {
        for command in [
            "rm -rf build",
            "cd src && mv a.rs b.rs",
            "sudo cp x y",
            "FOO=1 /bin/rm file",
            "sed -i 's/a/b/' notes.txt",
            "sed --in-place=.bak s/a/b/ f",
            "echo hi > out.txt",
            "cat a >> log.txt",
            "git reset --hard HEAD~1",
            "git -C repo clean -fdx",
            "git checkout -- src/lib.rs",
            "git restore .",
            "dd if=/dev/zero of=disk.img bs=1M count=1",
            "truncate -s 0 big.log",
        ] {
            assert!(is_destructive(command), "{command}");
        }
    }

    #[test]
    fn ordinary_commands_are_not() {
        for command in [
            "ls -la",
            "cargo test 2>&1",
            "grep rm notes.txt",
            "echo 'a > b'",
            "sed 's/a/b/' notes.txt",
            "make > /dev/null",
            "git status",
            "git log --oneline",
            "echo rm",
        ] {
            assert!(!is_destructive(command), "{command}");
        }
    }
}
