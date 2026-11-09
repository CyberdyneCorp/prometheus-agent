//! `read_file` document extraction (spec: execution-environments,
//! `read_file` document extraction).

use std::path::Path;
use std::sync::Arc;

use pmt_tools::files::documents::{ConvertError, Converted, DocConverter};
use pmt_tools::files::{FileSession, Linter, ReadArgs};
use pmt_tools::result::Status;

use crate::support::{Fixture, body};

/// A `doc-converter` that returns fixed pages and records what it read.
struct FakeConverter {
    pages: Vec<String>,
}

impl DocConverter for FakeConverter {
    fn convert(&self, path: &Path, ext: &str) -> Result<Converted, ConvertError> {
        assert!(path.is_file(), "converted on this host: {}", path.display());
        assert_eq!(ext, "pdf");
        Ok(Converted::Pages(self.pages.clone()))
    }
}

fn with_converter(fx: &Fixture, pages: Vec<String>) -> FileSession {
    let mut setup = fx.setup(Linter::builtin(), &[]);
    setup.documents = Some(Arc::new(FakeConverter { pages }));
    FileSession::new(setup)
}

fn content(session: &FileSession, path: &str) -> String {
    let result = session.read_file(&ReadArgs::new(path));
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    body(&result)["content"].as_str().unwrap().to_owned()
}

/// Scenario: SQLite schema overview.
#[test]
fn a_live_sqlite_database_is_summarised_without_being_modified() {
    let fx = Fixture::new();
    let path = fx.path("app.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, total REAL);
         CREATE INDEX orders_by_user ON orders (user_id);
         CREATE VIEW big AS SELECT * FROM orders WHERE total > 100;",
    )
    .unwrap();
    for n in 1..=7 {
        conn.execute("INSERT INTO users (name) VALUES (?1)", [format!("user{n}")])
            .unwrap();
        conn.execute(
            "INSERT INTO orders (user_id, total) VALUES (?1, ?1 * 50)",
            [n],
        )
        .unwrap();
    }
    // A checkpoint puts the rows in the main file; the connection stays
    // open (a live database).
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();

    let text = content(&fx.session(), "app.db");
    assert!(
        text.contains("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)"),
        "{text}"
    );
    assert!(text.contains("CREATE TABLE orders"), "{text}");
    assert!(text.contains("## Table users (7 rows)"), "{text}");
    assert!(text.contains("## Table orders (7 rows)"), "{text}");
    assert!(text.contains("| 5 | user5 |"), "{text}");
    assert!(!text.contains("user6"), "at most 5 rows per table: {text}");
    assert!(text.contains("`orders_by_user` on `orders`"), "{text}");
    assert!(text.contains("`big` on `big`"), "{text}");

    assert_eq!(std::fs::read(&path).unwrap(), before, "app.db is unchanged");
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        modified
    );
    drop(conn);
}

/// Scenario: Scanned PDF.
#[test]
fn a_scanned_pdf_starts_with_the_coverage_warning() {
    let fx = Fixture::new();
    fx.write("report.pdf", b"%PDF-1.7\n\x00\x01binary");
    let pages = (1..=311)
        .map(|n| {
            if (42..=77).contains(&n) {
                String::new()
            } else {
                format!("Findings of page {n}.")
            }
        })
        .collect();
    let text = content(&with_converter(&fx, pages), "report.pdf");
    assert!(
        text.starts_with(
            "     1|[EXTRACTION COVERAGE WARNING: 36 of 311 pages in this PDF yielded no text"
        ),
        "{}",
        &text[..300]
    );
    assert!(
        text.contains("pages 42-77 (36 pages), after: \"Findings of page 41.\""),
        "{}",
        &text[..600]
    );
}

#[test]
fn without_the_runtime_a_pdf_keeps_the_binary_refusal_with_an_install_hint() {
    let fx = Fixture::new();
    fx.write("report.pdf", b"%PDF-1.7\n\x00\x01binary");
    let result = fx.session().read_file(&ReadArgs::new("report.pdf"));
    let text = result.text_content();
    assert!(
        text.contains("Cannot read binary file 'report.pdf'"),
        "{text}"
    );
    assert!(
        text.contains("install the doc-converter runtime to read .pdf files"),
        "{text}"
    );
}

#[test]
fn a_text_file_named_db_is_read_as_text() {
    let fx = Fixture::new();
    fx.write("notes.db", "plain text\n");
    assert_eq!(content(&fx.session(), "notes.db"), "     1|plain text\n");
}
