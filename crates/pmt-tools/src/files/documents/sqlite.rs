//! A schema overview of a SQLite database: each table's `CREATE`
//! statement, row count and first rows, then its indexes, views and
//! triggers. The database is opened read-only and immutable, so reading
//! a live database never writes to it (no journal, no `-shm` file).

use std::fmt::Write as _;
use std::path::Path;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};

/// Rows shown per table.
const SAMPLE_ROWS: usize = 5;
/// Characters shown per value.
const VALUE_CHARS: usize = 200;

/// The overview of the database in `local` (this host) or in `bytes` (a
/// remote backend's file, copied to a private temporary file first).
pub(super) fn render(shown: &str, local: Option<&Path>, bytes: &[u8]) -> Result<String, String> {
    if let Some(path) = local {
        return overview(shown, path);
    }
    let copy =
        super::TempCopy::new(bytes, "db").map_err(|e| format!("cannot stage '{shown}': {e}"))?;
    overview(shown, copy.path())
}

fn overview(shown: &str, path: &Path) -> Result<String, String> {
    let conn = open(path).map_err(|e| format!("cannot open '{shown}' as SQLite: {e}"))?;
    describe(&conn, shown).map_err(|e| format!("cannot read '{shown}': {e}"))
}

/// Characters kept as they are in a URI path.
const URI_PATH: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'/')
    .remove(b':')
    .remove(b'-')
    .remove(b'_')
    .remove(b'.');

/// Opens `path` with `mode=ro&immutable=1` (`file:///C:/...` on Windows).
fn open(path: &Path) -> rusqlite::Result<Connection> {
    let slashed = path.to_string_lossy().replace('\\', "/");
    let encoded = percent_encoding::utf8_percent_encode(&slashed, URI_PATH).to_string();
    let lead = if encoded.starts_with('/') { "" } else { "/" };
    let uri = format!("file://{lead}{encoded}?mode=ro&immutable=1");
    Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
}

fn describe(conn: &Connection, shown: &str) -> rusqlite::Result<String> {
    let mut text = format!("# SQLite database {shown}\n");
    let objects = schema(conn)?;
    for (kind, name, _, sql) in &objects {
        if kind == "table" && !name.starts_with("sqlite_") {
            table_section(conn, &mut text, name, sql)?;
        }
    }
    for (kind, title) in [
        ("index", "Indexes"),
        ("view", "Views"),
        ("trigger", "Triggers"),
    ] {
        let rows: Vec<_> = objects.iter().filter(|o| o.0 == kind).collect();
        let _ = write!(text, "\n## {title}\n\n");
        if rows.is_empty() {
            text.push_str("(none)\n");
        }
        for (_, name, table, sql) in rows {
            let _ = writeln!(text, "- `{name}` on `{table}`: {}", one_line(sql));
        }
    }
    Ok(text)
}

/// Every schema object: kind, name, table and SQL (autoindexes have none).
fn schema(conn: &Connection) -> rusqlite::Result<Vec<(String, String, String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT type, name, tbl_name, COALESCE(sql, '') FROM sqlite_schema ORDER BY type, name",
    )?;
    stmt.query_map([], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    })?
    .collect()
}

fn table_section(
    conn: &Connection,
    text: &mut String,
    name: &str,
    sql: &str,
) -> rusqlite::Result<()> {
    let quoted = format!("\"{}\"", name.replace('"', "\"\""));
    let count: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {quoted}"), [], |r| r.get(0))?;
    let _ = write!(
        text,
        "\n## Table {name} ({count} rows)\n\n```sql\n{sql}\n```\n"
    );
    let mut stmt = conn.prepare(&format!("SELECT * FROM {quoted} LIMIT {SAMPLE_ROWS}"))?;
    let columns: Vec<String> = stmt.column_names().map(ToOwned::to_owned).collect();
    if columns.is_empty() || count == 0 {
        return Ok(());
    }
    let _ = write!(
        text,
        "\nFirst rows:\n\n| {} |\n|{}\n",
        columns.join(" | "),
        " --- |".repeat(columns.len())
    );
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let cells: Vec<String> = (0..columns.len())
            .map(|i| row.get_ref(i).map(value).unwrap_or_default())
            .collect();
        let _ = writeln!(text, "| {} |", cells.join(" | "));
    }
    Ok(())
}

fn value(value: ValueRef<'_>) -> String {
    let text = match value {
        ValueRef::Null => "NULL".to_owned(),
        ValueRef::Integer(n) => n.to_string(),
        ValueRef::Real(x) => x.to_string(),
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
        ValueRef::Blob(b) => format!("<blob {} bytes>", b.len()),
    };
    let flat = one_line(&text).replace('|', "\\|");
    if flat.chars().count() > VALUE_CHARS {
        let kept: String = flat.chars().take(VALUE_CHARS).collect();
        format!("{kept}...")
    } else {
        flat
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
