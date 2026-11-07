//! `.docx` and `.xlsx` as Markdown, read from their zipped XML parts.
//!
//! Word: paragraphs (headings from `HeadingN` styles) and tables in body
//! order. Excel: one table per sheet, the first row as the header, at most
//! [`MAX_ROWS`] rows and [`MAX_COLUMNS`] columns per sheet.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{Cursor, Read as _};

use roxmltree::{Document, Node};

/// Rows shown per sheet.
pub(super) const MAX_ROWS: usize = 5_000;
/// Columns shown per sheet.
pub(super) const MAX_COLUMNS: usize = 256;
/// Largest XML part read (bytes).
const MAX_PART_BYTES: u64 = 64 * 1024 * 1024;

const WORD: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const RELS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

type Archive<'a> = zip::ZipArchive<Cursor<&'a [u8]>>;

fn archive(bytes: &[u8]) -> Result<Archive<'_>, String> {
    zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("not an Office file: {e}"))
}

/// The text of part `name`, if present.
fn part(archive: &mut Archive<'_>, name: &str) -> Result<Option<String>, String> {
    let file = match archive.by_name(name) {
        Ok(file) => file,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(format!("cannot read {name}: {e}")),
    };
    let mut text = String::new();
    file.take(MAX_PART_BYTES)
        .read_to_string(&mut text)
        .map_err(|e| format!("cannot read {name}: {e}"))?;
    Ok(Some(text))
}

fn parse(text: &str) -> Result<Document<'_>, String> {
    Document::parse(text).map_err(|e| format!("malformed XML: {e}"))
}

fn is(node: Node<'_, '_>, namespace: &str, name: &str) -> bool {
    node.tag_name().namespace() == Some(namespace) && node.tag_name().name() == name
}

/// A Word document's body as Markdown.
pub(super) fn docx(bytes: &[u8]) -> Result<String, String> {
    let mut archive = archive(bytes)?;
    let xml = part(&mut archive, "word/document.xml")?
        .ok_or("not a Word document: word/document.xml is missing")?;
    let doc = parse(&xml)?;
    let body = doc
        .descendants()
        .find(|n| is(*n, WORD, "body"))
        .ok_or("not a Word document: no body")?;
    let mut text = String::new();
    for child in body.children() {
        if is(child, WORD, "p") {
            let line = paragraph(child);
            let level = heading_level(child);
            match level {
                Some(level) if !line.is_empty() => {
                    let _ = write!(text, "{} {line}\n\n", "#".repeat(level));
                }
                _ if !line.is_empty() => {
                    let _ = write!(text, "{line}\n\n");
                }
                _ => {}
            }
        } else if is(child, WORD, "tbl") {
            table(&mut text, child);
        }
    }
    Ok(text)
}

/// The text of one paragraph (runs, tabs and breaks).
fn paragraph(node: Node<'_, '_>) -> String {
    let mut line = String::new();
    for n in node.descendants() {
        if is(n, WORD, "t") {
            line.push_str(n.text().unwrap_or_default());
        } else if is(n, WORD, "tab") {
            line.push('\t');
        } else if is(n, WORD, "br") {
            line.push('\n');
        }
    }
    line.trim().to_owned()
}

/// `Heading<N>` / `Title` paragraph styles as a Markdown heading level.
fn heading_level(node: Node<'_, '_>) -> Option<usize> {
    let style = node
        .descendants()
        .find(|n| is(*n, WORD, "pStyle"))?
        .attribute((WORD, "val"))?;
    if style.eq_ignore_ascii_case("title") {
        return Some(1);
    }
    let digits = style
        .strip_prefix("Heading")
        .or_else(|| style.strip_prefix("heading"))?;
    digits.parse::<usize>().ok().map(|n| n.clamp(1, 6))
}

fn table(text: &mut String, node: Node<'_, '_>) {
    let rows: Vec<Vec<String>> = node
        .children()
        .filter(|n| is(*n, WORD, "tr"))
        .map(|row| {
            row.children()
                .filter(|n| is(*n, WORD, "tc"))
                .map(|cell| {
                    cell.children()
                        .filter(|n| is(*n, WORD, "p"))
                        .map(paragraph)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect()
        })
        .collect();
    markdown_table(text, &rows);
}

fn markdown_table(text: &mut String, rows: &[Vec<String>]) {
    let Some(width) = rows.iter().map(Vec::len).max().filter(|w| *w > 0) else {
        return;
    };
    for (index, row) in rows.iter().enumerate() {
        let cells: Vec<String> = (0..width)
            .map(|i| cell(row.get(i).map_or("", String::as_str)))
            .collect();
        let _ = writeln!(text, "| {} |", cells.join(" | "));
        if index == 0 {
            let _ = writeln!(text, "|{}", " --- |".repeat(width));
        }
    }
    text.push('\n');
}

fn cell(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

/// Every sheet of a workbook as a Markdown table.
pub(super) fn xlsx(bytes: &[u8]) -> Result<String, String> {
    let mut archive = archive(bytes)?;
    let workbook = part(&mut archive, "xl/workbook.xml")?
        .ok_or("not an Excel workbook: xl/workbook.xml is missing")?;
    let rels = part(&mut archive, "xl/_rels/workbook.xml.rels")?.unwrap_or_default();
    let shared = part(&mut archive, "xl/sharedStrings.xml")?
        .map(|xml| shared_strings(&xml))
        .transpose()?
        .unwrap_or_default();
    let targets = relationships(&rels)?;
    let doc = parse(&workbook)?;
    let mut text = String::new();
    for sheet in doc.descendants().filter(|n| n.tag_name().name() == "sheet") {
        let name = sheet.attribute("name").unwrap_or("Sheet");
        let target = sheet
            .attribute((RELS, "id"))
            .and_then(|id| targets.get(id))
            .map(|t| sheet_path(t));
        let Some(xml) = target
            .map(|t| part(&mut archive, &t))
            .transpose()?
            .flatten()
        else {
            continue;
        };
        let _ = write!(text, "## Sheet: {name}\n\n");
        sheet_table(&mut text, &xml, &shared)?;
    }
    Ok(text)
}

fn sheet_path(target: &str) -> String {
    match target.strip_prefix('/') {
        Some(absolute) => absolute.to_owned(),
        None => format!("xl/{target}"),
    }
}

fn relationships(xml: &str) -> Result<HashMap<String, String>, String> {
    if xml.is_empty() {
        return Ok(HashMap::new());
    }
    let doc = parse(xml)?;
    Ok(doc
        .descendants()
        .filter(|n| n.tag_name().name() == "Relationship")
        .filter_map(|n| {
            Some((
                n.attribute("Id")?.to_owned(),
                n.attribute("Target")?.to_owned(),
            ))
        })
        .collect())
}

fn shared_strings(xml: &str) -> Result<Vec<String>, String> {
    let doc = parse(xml)?;
    Ok(doc
        .root_element()
        .children()
        .filter(|n| n.tag_name().name() == "si")
        .map(|si| {
            si.descendants()
                .filter(|n| n.tag_name().name() == "t")
                .filter_map(|n| n.text())
                .collect()
        })
        .collect())
}

fn sheet_table(text: &mut String, xml: &str, shared: &[String]) -> Result<(), String> {
    let doc = parse(xml)?;
    let all: Vec<Node<'_, '_>> = doc
        .descendants()
        .filter(|n| n.tag_name().name() == "row")
        .collect();
    let mut rows = Vec::new();
    for row in all.iter().take(MAX_ROWS) {
        let mut cells: Vec<String> = Vec::new();
        for c in row.children().filter(|n| n.tag_name().name() == "c") {
            let column = c.attribute("r").map_or(cells.len(), column_index);
            if column >= MAX_COLUMNS {
                continue;
            }
            if cells.len() <= column {
                cells.resize(column + 1, String::new());
            }
            cells[column] = cell_value(c, shared);
        }
        rows.push(cells);
    }
    markdown_table(text, &rows);
    if all.len() > MAX_ROWS {
        let _ = write!(text, "[{} more rows not shown]\n\n", all.len() - MAX_ROWS);
    }
    Ok(())
}

/// The 0-based column of a cell reference such as `AB12`.
fn column_index(reference: &str) -> usize {
    reference
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .fold(0usize, |acc, c| {
            acc * 26 + (c.to_ascii_uppercase() as usize - 'A' as usize + 1)
        })
        .saturating_sub(1)
}

fn cell_value(c: Node<'_, '_>, shared: &[String]) -> String {
    let value = || {
        c.children()
            .find(|n| n.tag_name().name() == "v")
            .and_then(|v| v.text())
            .unwrap_or_default()
    };
    match c.attribute("t") {
        Some("s") => value()
            .parse::<usize>()
            .ok()
            .and_then(|i| shared.get(i).cloned())
            .unwrap_or_default(),
        Some("inlineStr") => c
            .descendants()
            .filter(|n| n.tag_name().name() == "t")
            .filter_map(|n| n.text())
            .collect(),
        Some("b") => if value() == "1" { "TRUE" } else { "FALSE" }.to_owned(),
        _ => value().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    pub(crate) fn zipped(parts: &[(&str, &str)]) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(&mut out);
        for (name, body) in parts {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        writer.finish().unwrap();
        out.into_inner()
    }

    #[test]
    fn a_word_document_keeps_headings_paragraphs_and_tables() {
        let xml = format!(
            r#"<w:document xmlns:w="{WORD}"><w:body>
            <w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr><w:r><w:t>Plan</w:t></w:r></w:p>
            <w:p><w:r><w:t>First</w:t></w:r><w:r><w:t xml:space="preserve"> step</w:t></w:r></w:p>
            <w:tbl><w:tr><w:tc><w:p><w:r><w:t>a</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>b</w:t></w:r></w:p></w:tc></w:tr>
            <w:tr><w:tc><w:p><w:r><w:t>1</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>2</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
            </w:body></w:document>"#
        );
        let text = docx(&zipped(&[("word/document.xml", &xml)])).unwrap();
        assert_eq!(
            text,
            "## Plan\n\nFirst step\n\n| a | b |\n| --- | --- |\n| 1 | 2 |\n\n"
        );
    }

    #[test]
    fn a_workbook_becomes_one_table_per_sheet() {
        let workbook = format!(
            r#"<workbook xmlns:r="{RELS}"><sheets><sheet name="Data" sheetId="1" r:id="rId1"/></sheets></workbook>"#
        );
        let rels = r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#;
        let shared = r"<sst><si><t>name</t></si><si><t>qty</t></si><si><t>bolt</t></si></sst>";
        let sheet = r#"<worksheet><sheetData>
            <row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c></row>
            <row r="2"><c r="A2" t="s"><v>2</v></c><c r="C2"><v>7</v></c></row>
            </sheetData></worksheet>"#;
        let bytes = zipped(&[
            ("xl/workbook.xml", &workbook),
            ("xl/_rels/workbook.xml.rels", rels),
            ("xl/sharedStrings.xml", shared),
            ("xl/worksheets/sheet1.xml", sheet),
        ]);
        let text = xlsx(&bytes).unwrap();
        assert_eq!(
            text,
            "## Sheet: Data\n\n| name | qty |  |\n| --- | --- | --- |\n| bolt |  | 7 |\n\n"
        );
    }

    #[test]
    fn column_references_are_decoded() {
        assert_eq!(column_index("A1"), 0);
        assert_eq!(column_index("Z9"), 25);
        assert_eq!(column_index("AA3"), 26);
    }
}
