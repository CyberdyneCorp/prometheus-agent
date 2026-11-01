//! Golden files: `UPDATE_GOLDEN=1` rewrites them from the current output.

use std::fs;
use std::path::PathBuf;

/// Compares `actual` with `golden/<name>`.
pub(crate) fn check(name: &str, actual: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/file_tools/golden")
        .join(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        fs::write(&path, actual).unwrap();
        return;
    }
    let expected = fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing golden file {}; run with UPDATE_GOLDEN=1",
            path.display()
        )
    });
    assert_eq!(
        actual,
        expected.replace("\r\n", "\n"),
        "golden {name} differs"
    );
}
