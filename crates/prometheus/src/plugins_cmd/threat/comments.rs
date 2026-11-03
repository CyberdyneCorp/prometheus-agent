//! Which lines of a plugin file are comments, by the file's language.
//!
//! A comment finding is lowered one step, so a wrong "comment" answer lets
//! executable code past the `dangerous` refusal. The rules are therefore
//! strict: the language comes from the file extension (unknown languages
//! have no comments), the whole line must be a comment, and a block comment
//! counts only when it closes on the same line with nothing after it. Lines
//! inside a multi-line block comment, and lines holding a character another
//! interpreter reads as a line end, are not recognised; they keep their
//! severity, which fails closed.

use std::path::Path;

/// The comment syntax of one language.
struct Syntax {
    /// Markers that comment out the rest of the line.
    line: &'static [&'static str],
    /// A block comment's opening and closing markers.
    block: Option<(&'static str, &'static str)>,
}

const HASH: Syntax = Syntax {
    line: &["#"],
    block: None,
};
const SLASHES: Syntax = Syntax {
    line: &["//"],
    block: Some(("/*", "*/")),
};
const CSS: Syntax = Syntax {
    line: &[],
    block: Some(("/*", "*/")),
};
// `--[[ x ]] code` runs `code` in Lua, so every `--[` line is treated as a
// block that must close (on `]]`) with nothing after it.
const LUA: Syntax = Syntax {
    line: &["--"],
    block: Some(("--[", "]]")),
};
const SQL: Syntax = Syntax {
    line: &["--"],
    block: Some(("/*", "*/")),
};
const SEMICOLON: Syntax = Syntax {
    line: &[";"],
    block: None,
};
const INI: Syntax = Syntax {
    line: &[";", "#"],
    block: None,
};
const MARKUP: Syntax = Syntax {
    line: &[],
    block: Some(("<!--", "-->")),
};

/// Languages by extension. PHP (`?>` ends a line comment) and batch files
/// (`rem` parsing quirks) are left out on purpose: they have no comments.
const LANGUAGES: &[(&[&str], Syntax)] = &[
    (
        &[
            "sh", "bash", "zsh", "fish", "ksh", "py", "pyi", "pyw", "rb", "pl", "pm", "r", "ps1",
            "psm1", "toml", "yaml", "yml", "conf", "cfg", "nix", "tcl", "awk",
        ],
        HASH,
    ),
    (
        &[
            "js", "mjs", "cjs", "jsx", "ts", "mts", "cts", "tsx", "rs", "go", "c", "h", "cc",
            "cpp", "cxx", "hpp", "java", "kt", "kts", "swift", "scala", "cs", "dart", "zig", "wit",
            "proto", "jsonc", "json5", "scss", "less",
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

/// Whether line `line` (1-based) of `file`, whose contents are `text`, is
/// entirely a comment.
pub(super) fn is_comment(file: &str, text: &str, line: usize) -> bool {
    let Some(syntax) = syntax(file) else {
        return false;
    };
    // A source encoding such as UTF-7 can turn comment bytes into code
    // (`# +AAo-...` decodes to a newline), so such files have no comments.
    if declares_foreign_encoding(text) {
        return false;
    }
    text.lines()
        .nth(line.saturating_sub(1))
        .is_some_and(|line| !has_foreign_terminator(line) && whole_line_comment(syntax, line))
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

fn declares_foreign_encoding(text: &str) -> bool {
    text.lines().take(2).any(|line| {
        let line = line.to_ascii_lowercase();
        line.contains("coding") && !line.contains("utf-8") && !line.contains("utf8")
    })
}

fn whole_line_comment(syntax: &Syntax, line: &str) -> bool {
    let line = line.trim();
    // A shebang names the interpreter that runs the file.
    if line.starts_with("#!") {
        return false;
    }
    if let Some((open, close)) = syntax.block
        && let Some(rest) = line.strip_prefix(open)
    {
        return rest
            .find(close)
            .is_some_and(|end| rest[end + close.len()..].trim().is_empty());
    }
    syntax.line.iter().any(|marker| line.starts_with(marker))
}

#[cfg(test)]
mod tests {
    use super::is_comment;

    fn first(file: &str, line: &str) -> bool {
        is_comment(file, line, 1)
    }

    #[test]
    fn a_whole_line_comment_in_the_files_language_is_a_comment() {
        assert!(first("run.sh", "  # bash -i"));
        assert!(first("setup.py", "# os.system('x')"));
        assert!(first("index.js", "// require('child_process')"));
        assert!(first("index.js", "/* execSync('x') */"));
        assert!(first("init.lua", "-- os.execute('x')"));
        assert!(first("init.lua", "--[[ os.execute('x') ]]"));
        assert!(first("page.html", "<!-- curl x | sh -->"));
    }

    #[test]
    fn code_after_a_closed_block_comment_is_not_a_comment() {
        assert!(!first(
            "setup.js",
            "/**/ require('child_process').execSync('x');"
        ));
        assert!(!first("init.lua", "--[[ x ]] os.execute('x')"));
        assert!(!first("page.html", "<!-- x --> <script>x</script>"));
        assert!(!first("index.js", "/* never closed execSync('x')"));
    }

    #[test]
    fn markers_of_other_languages_are_code() {
        assert!(!first("setup.py", "*_, = [__import__('os').system('x')]"));
        assert!(!first("setup.py", "// x"));
        assert!(!first("setup.js", "# x"));
        assert!(!first("setup.js", "-- x"));
        assert!(!first("setup.js", "; x"));
        assert!(!first("setup.sh", "rem x"));
        assert!(!first("tool.php", "// x ?> <?php system('x');"));
        assert!(!first("install", "# no extension, no language"));
    }

    #[test]
    fn a_line_holding_another_line_terminator_is_not_a_comment() {
        // Python ends a line at a lone CR (and VT, FF); JS at U+2028/U+2029.
        for sep in ["\r", "\u{b}", "\u{c}", "\u{85}", "\u{2028}", "\u{2029}"] {
            assert!(!first("setup.py", &format!("# x{sep}import os")), "{sep:?}");
            assert!(
                !first("setup.js", &format!("// x{sep}execSync('x')")),
                "{sep:?}"
            );
        }
        assert!(is_comment("setup.py", "# x\r\nimport os\r\n", 1));
    }

    #[test]
    fn shebangs_and_foreign_source_encodings_are_not_comments() {
        assert!(!first("run.sh", "#!/bin/sh -c 'x'"));
        let utf7 = "# -*- coding: utf-7 -*-\n# +AAo-import os; os.system('x')\n";
        assert!(!is_comment("setup.py", utf7, 2));
        let utf8 = "# -*- coding: utf-8 -*-\n# import os\n";
        assert!(is_comment("setup.py", utf8, 2));
    }
}
