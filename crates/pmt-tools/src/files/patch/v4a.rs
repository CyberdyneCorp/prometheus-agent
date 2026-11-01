//! The V4A multi-file patch format (spec: execution-environments, patch
//! tool contract).
//!
//! ```text
//! *** Begin Patch
//! *** Add File: new.rs
//! +fn new() {}
//! *** Update File: src/a.rs
//! *** Move to: src/b.rs
//! @@ fn context_line()
//!  unchanged
//! -removed
//! +added
//! *** Delete File: old.rs
//! *** End Patch
//! ```
//!
//! Parsing is pure; [`apply_hunks`] computes a file's new text in memory, so
//! the caller can check every file before writing any.

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
const ADD: &str = "*** Add File: ";
const UPDATE: &str = "*** Update File: ";
const DELETE: &str = "*** Delete File: ";
const MOVE: &str = "*** Move to: ";
const END_OF_FILE: &str = "*** End of File";

/// One file operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Create `path` with `content`.
    Add {
        /// The new file.
        path: String,
        /// Its content.
        content: String,
    },
    /// Remove `path`.
    Delete {
        /// The removed file.
        path: String,
    },
    /// Apply hunks to `path`, optionally renaming it.
    Update {
        /// The edited file.
        path: String,
        /// The new name, when renamed.
        move_to: Option<String>,
        /// The edits, in file order.
        hunks: Vec<Hunk>,
    },
}

impl Op {
    /// The file the operation starts from.
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Self::Add { path, .. } | Self::Delete { path } | Self::Update { path, .. } => path,
        }
    }
}

/// One `@@` section.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hunk {
    /// Text after `@@`: a line to find before the hunk's context.
    pub header: Option<String>,
    /// Context (` `), removed (`-`) and added (`+`) lines.
    pub lines: Vec<Line>,
}

/// One hunk line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// Kept as is.
    Context(String),
    /// Removed.
    Remove(String),
    /// Added.
    Add(String),
}

impl Hunk {
    fn old(&self) -> Vec<&str> {
        self.lines
            .iter()
            .filter_map(|line| match line {
                Line::Context(text) | Line::Remove(text) => Some(text.as_str()),
                Line::Add(_) => None,
            })
            .collect()
    }

    fn new_lines(&self) -> Vec<&str> {
        self.lines
            .iter()
            .filter_map(|line| match line {
                Line::Context(text) | Line::Add(text) => Some(text.as_str()),
                Line::Remove(_) => None,
            })
            .collect()
    }
}

/// Every path a patch adds, updates, deletes or moves to, read from its
/// headers alone (the approval gate's write claims).
#[must_use]
pub fn touched_paths(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            [ADD, UPDATE, DELETE, MOVE]
                .iter()
                .find_map(|prefix| line.strip_prefix(prefix))
                .map(|path| path.trim().to_owned())
        })
        .collect()
}

/// Parses a whole patch.
///
/// # Errors
///
/// What is malformed, with the line number.
pub fn parse(text: &str) -> Result<Vec<Op>, String> {
    let lines: Vec<&str> = text
        .trim()
        .lines()
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    if lines.first().map(|l| l.trim()) != Some(BEGIN) {
        return Err(format!("the patch must start with '{BEGIN}'"));
    }
    if lines.last().map(|l| l.trim()) != Some(END) {
        return Err(format!("the patch must end with '{END}'"));
    }
    let body = &lines[1..lines.len() - 1];
    let mut ops = Vec::new();
    let mut index = 0;
    while index < body.len() {
        let (op, next) = parse_op(body, index)?;
        ops.push(op);
        index = next;
    }
    if ops.is_empty() {
        return Err("the patch changes no file".to_owned());
    }
    Ok(ops)
}

/// One operation starting at `body[index]`; returns it and the index of
/// the next operation header.
fn parse_op(body: &[&str], index: usize) -> Result<(Op, usize), String> {
    let line = body[index];
    let number = index + 2;
    let end = next_header(body, index + 1);
    if let Some(path) = line.strip_prefix(ADD) {
        let content = add_content(&body[index + 1..end], number)?;
        return Ok((
            Op::Add {
                path: path.trim().to_owned(),
                content,
            },
            end,
        ));
    }
    if let Some(path) = line.strip_prefix(DELETE) {
        return Ok((
            Op::Delete {
                path: path.trim().to_owned(),
            },
            end,
        ));
    }
    if let Some(path) = line.strip_prefix(UPDATE) {
        let mut rest = index + 1;
        let move_to = body.get(rest).and_then(|l| l.strip_prefix(MOVE)).map(|to| {
            rest += 1;
            to.trim().to_owned()
        });
        let hunks = parse_hunks(&body[rest..end], rest + 2)?;
        let op = Op::Update {
            path: path.trim().to_owned(),
            move_to,
            hunks,
        };
        return Ok((op, end));
    }
    Err(format!(
        "line {number}: expected an '*** Add File:', '*** Update File:' or '*** Delete File:' header, found '{line}'"
    ))
}

fn next_header(body: &[&str], from: usize) -> usize {
    (from..body.len())
        .find(|&i| [ADD, UPDATE, DELETE].iter().any(|p| body[i].starts_with(p)))
        .unwrap_or(body.len())
}

fn add_content(lines: &[&str], header_line: usize) -> Result<String, String> {
    let mut content = String::new();
    for (offset, line) in lines.iter().enumerate() {
        let Some(text) = line.strip_prefix('+') else {
            return Err(format!(
                "line {}: every line of an added file must start with '+'",
                header_line + offset + 1
            ));
        };
        content.push_str(text);
        content.push('\n');
    }
    Ok(content)
}

fn parse_hunks(lines: &[&str], first_line: usize) -> Result<Vec<Hunk>, String> {
    let mut hunks: Vec<Hunk> = Vec::new();
    for (offset, line) in lines.iter().enumerate() {
        if line.trim() == END_OF_FILE {
            continue;
        }
        if let Some(header) = line.strip_prefix("@@") {
            let header = header.trim();
            hunks.push(Hunk {
                header: (!header.is_empty()).then(|| header.to_owned()),
                lines: Vec::new(),
            });
            continue;
        }
        let parsed = match line.chars().next() {
            Some(' ') | None => Line::Context(line.get(1..).unwrap_or_default().to_owned()),
            Some('-') => Line::Remove(line[1..].to_owned()),
            Some('+') => Line::Add(line[1..].to_owned()),
            Some(_) => {
                return Err(format!(
                    "line {}: hunk lines must start with ' ', '-' or '+', found '{line}'",
                    first_line + offset
                ));
            }
        };
        if hunks.is_empty() {
            hunks.push(Hunk::default());
        }
        if let Some(hunk) = hunks.last_mut() {
            hunk.lines.push(parsed);
        }
    }
    hunks.retain(|h| !h.lines.is_empty());
    if hunks.is_empty() {
        return Err("an '*** Update File:' section has no changes".to_owned());
    }
    Ok(hunks)
}

/// Applies `hunks` in order to `content`.
///
/// # Errors
///
/// `hunk <n> does not apply: ...` for the first hunk whose context is not
/// found after the previous hunk.
pub fn apply_hunks(content: &str, hunks: &[Hunk]) -> Result<String, String> {
    let trailing_newline = content.is_empty() || content.ends_with('\n');
    let mut lines: Vec<String> = super::super::lines(content)
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut cursor = 0;
    for (number, hunk) in hunks.iter().enumerate() {
        let start = match &hunk.header {
            Some(header) => find_header(&lines, cursor, header).ok_or_else(|| {
                format!(
                    "hunk {} does not apply: '@@ {header}' not found",
                    number + 1
                )
            })?,
            None => cursor,
        };
        let old = hunk.old();
        let at = if old.is_empty() {
            if hunk.header.is_some() {
                start
            } else {
                lines.len()
            }
        } else {
            locate(&lines, start, &old).ok_or_else(|| {
                format!(
                    "hunk {} does not apply: its context was not found:\n{}",
                    number + 1,
                    old.iter().take(5).copied().collect::<Vec<_>>().join("\n")
                )
            })?
        };
        let new: Vec<String> = hunk.new_lines().into_iter().map(str::to_owned).collect();
        let added = new.len();
        lines.splice(at..at + old.len(), new);
        cursor = at + added;
    }
    let mut text = lines.join("\n");
    if trailing_newline && !lines.is_empty() {
        text.push('\n');
    }
    Ok(text)
}

fn find_header(lines: &[String], from: usize, header: &str) -> Option<usize> {
    let wanted = header.trim();
    (from..lines.len())
        .find(|&i| lines[i].trim() == wanted)
        .or_else(|| (from..lines.len()).find(|&i| lines[i].contains(wanted)))
        .map(|i| i + 1)
}

/// The first index at or after `from` where `old` matches: exactly, then
/// ignoring trailing whitespace, then ignoring surrounding whitespace.
fn locate(lines: &[String], from: usize, old: &[&str]) -> Option<usize> {
    let tolerances: [fn(&str) -> &str; 3] = [|l| l, str::trim_end, str::trim];
    tolerances.iter().find_map(|norm| {
        (from..=lines.len().checked_sub(old.len())?).find(|&i| {
            old.iter()
                .zip(&lines[i..i + old.len()])
                .all(|(want, have)| norm(want) == norm(have))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::{Line, Op, apply_hunks, parse, touched_paths};

    const PATCH: &str = "*** Begin Patch
*** Add File: new.txt
+hello
*** Update File: a.rs
*** Move to: b.rs
@@ fn main() {
     let x = 1;
-    let y = 2;
+    let y = 3;
*** Delete File: old.rs
*** End Patch
";

    #[test]
    fn parses_every_operation() {
        let ops = parse(PATCH).unwrap();
        assert_eq!(ops.len(), 3);
        assert_eq!(
            ops[0],
            Op::Add {
                path: "new.txt".into(),
                content: "hello\n".into()
            }
        );
        let Op::Update {
            path,
            move_to,
            hunks,
        } = &ops[1]
        else {
            panic!("{ops:?}")
        };
        assert_eq!((path.as_str(), move_to.as_deref()), ("a.rs", Some("b.rs")));
        assert_eq!(hunks[0].header.as_deref(), Some("fn main() {"));
        assert_eq!(hunks[0].lines[1], Line::Remove("    let y = 2;".into()));
        assert_eq!(
            ops[2],
            Op::Delete {
                path: "old.rs".into()
            }
        );
        assert_eq!(touched_paths(PATCH), ["new.txt", "a.rs", "b.rs", "old.rs"]);
    }

    #[test]
    fn malformed_patches_are_refused() {
        assert!(
            parse("*** Update File: a\n")
                .unwrap_err()
                .contains("Begin Patch")
        );
        assert!(parse("*** Begin Patch\n*** Frobnicate: a\n*** End Patch").is_err());
        assert!(parse("*** Begin Patch\n*** Add File: a\nno plus\n*** End Patch").is_err());
    }

    #[test]
    fn hunks_apply_in_order_and_report_the_failing_one() {
        let ops = parse(PATCH).unwrap();
        let Op::Update { hunks, .. } = &ops[1] else {
            unreachable!()
        };
        let file = "fn main() {\n    let x = 1;\n    let y = 2;\n}\n";
        assert_eq!(
            apply_hunks(file, hunks).unwrap(),
            "fn main() {\n    let x = 1;\n    let y = 3;\n}\n"
        );
        let err = apply_hunks("fn main() {\n}\n", hunks).unwrap_err();
        assert!(err.starts_with("hunk 1 does not apply"), "{err}");
    }
}
