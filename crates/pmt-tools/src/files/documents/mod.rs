//! `read_file` document extraction (spec: execution-environments,
//! `read_file` document extraction).
//!
//! Notebooks, `.docx`, `.xlsx` and SQLite files are converted here;
//! PDFs, legacy Office, `OpenDocument`, RTF and EPUB go through the managed
//! `doc-converter` runtime ([`DocConverter`]). The result is Markdown that
//! `read_file` paginates like any text file. Files of a remote backend
//! arrive as bytes (downloaded through the backend) and are converted on
//! this host.

mod notebook;
mod office;
mod pdf;
mod sqlite;

use std::path::Path;
use std::sync::Arc;

pub use pdf::coverage_warning;

/// Extensions converted by this crate.
const BUILT_IN: &[&str] = &["ipynb", "docx", "xlsx", "db", "sqlite", "sqlite3"];
/// Extensions converted by the `doc-converter` runtime.
const RUNTIME: &[&str] = &[
    "pdf", "doc", "xls", "ppt", "pptx", "odt", "ods", "odp", "rtf", "epub",
];
/// Largest document converted (bytes).
pub const MAX_BYTES: u64 = 50 * 1024 * 1024;

/// What the `doc-converter` runtime made of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Converted {
    /// Text per page (PDF), page 1 first.
    Pages(Vec<String>),
    /// The whole document as Markdown.
    Markdown(String),
}

/// The managed `doc-converter` runtime.
pub trait DocConverter: Send + Sync {
    /// Converts the file at `path` (extension `ext`, lower case).
    ///
    /// # Errors
    ///
    /// [`ConvertError::Unavailable`] when the runtime is missing and cannot
    /// be installed, [`ConvertError::Failed`] when the conversion failed.
    fn convert(&self, path: &Path, ext: &str) -> Result<Converted, ConvertError>;
}

/// Why the runtime converted nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvertError {
    /// The runtime is not installed and may not be installed now.
    Unavailable(String),
    /// The runtime ran and failed.
    Failed(String),
}

/// How a file is converted, by its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Converted here.
    BuiltIn(&'static str),
    /// Converted by the runtime.
    Runtime(&'static str),
}

/// The document kind of `path`, if it is one.
#[must_use]
pub fn kind_of(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if let Some(found) = BUILT_IN.iter().find(|e| **e == ext) {
        return Some(Kind::BuiltIn(found));
    }
    RUNTIME
        .iter()
        .find(|e| **e == ext)
        .map(|found| Kind::Runtime(found))
}

/// Whether `bytes` are a SQLite database (its 16-byte header).
#[must_use]
pub fn is_sqlite(bytes: &[u8]) -> bool {
    bytes.starts_with(b"SQLite format 3\0")
}

/// Converts a built-in document. `local` is the file on this host when
/// the backend uses it (SQLite is opened in place, read-only and
/// immutable); otherwise the bytes are converted.
///
/// # Errors
///
/// Why the document cannot be read.
pub fn built_in(
    ext: &str,
    shown: &str,
    local: Option<&Path>,
    bytes: &[u8],
) -> Result<String, String> {
    match ext {
        "ipynb" => notebook::render(shown, bytes),
        "docx" => office::docx(bytes),
        "xlsx" => office::xlsx(bytes),
        _ => sqlite::render(shown, local, bytes),
    }
}

/// Converts a runtime document with `converter`; PDFs get the extraction
/// coverage warning.
///
/// # Errors
///
/// The refusal `read_file` returns.
pub fn with_runtime(
    converter: Option<&Arc<dyn DocConverter>>,
    ext: &str,
    shown: &str,
    path: &Path,
) -> Result<String, String> {
    let missing = |why: &str| {
        format!(
            "Cannot read binary file '{shown}'; use vision_analyze for images or terminal for binary inspection; install the doc-converter runtime to read .{ext} files ({why})"
        )
    };
    let Some(converter) = converter else {
        return Err(missing("no runtime store"));
    };
    match converter.convert(path, ext) {
        Ok(Converted::Markdown(text)) => Ok(text),
        Ok(Converted::Pages(pages)) => Ok(pdf::render(&pages)),
        Err(ConvertError::Unavailable(why)) => Err(missing(&why)),
        Err(ConvertError::Failed(why)) => Err(format!("cannot convert '{shown}': {why}")),
    }
}

/// A private temporary copy of a remote backend's file, converted on this
/// host and removed on drop.
#[derive(Debug)]
pub struct TempCopy(std::path::PathBuf);

impl TempCopy {
    /// Writes `bytes` to a new owner-only file ending in `.<ext>`.
    ///
    /// # Errors
    ///
    /// The file cannot be created or written.
    pub fn new(bytes: &[u8], ext: &str) -> std::io::Result<Self> {
        use std::io::Write as _;
        let path = std::env::temp_dir().join(format!(
            "pmt-doc-{}.{ext}",
            ulid::Ulid::generate().to_string().to_lowercase()
        ));
        let mut file = pmt_core::private_fs::new_file_options().open(&path)?;
        let copy = Self(path);
        file.write_all(bytes)?;
        Ok(copy)
    }

    /// Where the copy is.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempCopy {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_pick_the_converter() {
        assert_eq!(kind_of(Path::new("a/B.DOCX")), Some(Kind::BuiltIn("docx")));
        assert_eq!(
            kind_of(Path::new("x.sqlite3")),
            Some(Kind::BuiltIn("sqlite3"))
        );
        assert_eq!(kind_of(Path::new("scan.pdf")), Some(Kind::Runtime("pdf")));
        assert_eq!(kind_of(Path::new("notes.txt")), None);
        assert_eq!(kind_of(Path::new("Makefile")), None);
    }

    #[test]
    fn a_missing_runtime_keeps_the_binary_refusal() {
        let refusal = with_runtime(None, "pdf", "a.pdf", Path::new("a.pdf")).unwrap_err();
        assert!(
            refusal.starts_with("Cannot read binary file 'a.pdf'"),
            "{refusal}"
        );
        assert!(refusal.contains("install the doc-converter runtime to read .pdf files"));
    }
}
