//! A unified line diff of two texts (`cron replay --live`, monitor
//! changes).

use std::fmt::Write as _;

/// The largest alignment table computed (lines old x lines new).
const MAX_CELLS: usize = 4_000_000;

/// The longest-common-subsequence lengths of every suffix pair.
fn lcs(old: &[&str], new: &[&str]) -> Vec<Vec<usize>> {
    let mut table = vec![vec![0_usize; new.len() + 1]; old.len() + 1];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            table[i][j] = if old[i] == new[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    table
}

/// The lines of `old` and `new` as a unified diff with labels `a` and
/// `b` (one hunk over the whole text; outputs are short).
#[must_use]
pub fn unified(old: &str, new: &str, a: &str, b: &str) -> String {
    if old == new {
        return "outputs are identical\n".to_owned();
    }
    let old: Vec<&str> = old.lines().collect();
    let new: Vec<&str> = new.lines().collect();
    if old.len().saturating_mul(new.len()) > MAX_CELLS {
        // Too large to align: every old line goes, every new one comes.
        let mut text = format!(
            "--- {a}\n+++ {b}\n@@ -1,{} +1,{} @@\n",
            old.len(),
            new.len()
        );
        for line in &old {
            let _ = writeln!(text, "-{line}");
        }
        for line in &new {
            let _ = writeln!(text, "+{line}");
        }
        return text;
    }
    let table = lcs(&old, &new);
    let mut text = format!(
        "--- {a}\n+++ {b}\n@@ -1,{} +1,{} @@\n",
        old.len(),
        new.len()
    );
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new.len() {
        let (mark, line) = if i < old.len() && j < new.len() && old[i] == new[j] {
            i += 1;
            j += 1;
            (' ', old[i - 1])
        } else if j < new.len() && (i == old.len() || table[i][j + 1] >= table[i + 1][j]) {
            j += 1;
            ('+', new[j - 1])
        } else {
            i += 1;
            ('-', old[i - 1])
        };
        let _ = writeln!(text, "{mark}{line}");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::unified;

    #[test]
    fn changed_lines_are_marked() {
        assert_eq!(
            unified("a\nb\nc", "a\nB\nc", "old", "new"),
            "--- old\n+++ new\n@@ -1,3 +1,3 @@\n a\n+B\n-b\n c\n"
        );
        assert_eq!(unified("x", "x", "o", "n"), "outputs are identical\n");
    }
}
