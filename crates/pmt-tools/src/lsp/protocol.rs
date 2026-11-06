//! The LSP wire format and the diagnostics model (spec: lsp-diagnostics,
//! Diagnostics introduced by an edit).
//!
//! Messages are JSON-RPC 2.0 bodies framed by a `Content-Length` header.
//! A diagnostic is compared by severity, message and code only: an edit
//! that shifts lines must not make every old diagnostic look new.

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt as _, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _,
};

/// The largest message body read from a server.
const MAX_BODY: usize = 64 * 1024 * 1024;

/// Reads one framed message; `Ok(None)` at end of stream.
///
/// # Errors
///
/// A malformed header, an oversized or non-JSON body, or I/O failure.
pub async fn read_message(
    reader: &mut (impl AsyncBufRead + Unpin),
) -> std::io::Result<Option<Value>> {
    let mut length = None;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let length = length
        .filter(|l| *l <= MAX_BODY)
        .ok_or_else(|| std::io::Error::other("missing or oversized Content-Length"))?;
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(std::io::Error::other)
}

/// Writes one framed message.
///
/// # Errors
///
/// I/O failure.
pub async fn write_message(
    writer: &mut (impl AsyncWrite + Unpin),
    message: &Value,
) -> std::io::Result<()> {
    let body = message.to_string();
    let frame = format!("Content-Length: {}\r\n\r\n{body}", body.len());
    writer.write_all(frame.as_bytes()).await?;
    writer.flush().await
}

/// `file://` URI of an absolute path.
#[must_use]
pub fn uri(path: &Path) -> String {
    url::Url::from_file_path(path)
        .map_or_else(|()| format!("file://{}", path.display()), |u| u.to_string())
}

/// One diagnostic as the model sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// 1 error, 2 warning, 3 information, 4 hint.
    pub severity: u8,
    /// 1-based line.
    pub line: u64,
    /// 1-based column.
    pub column: u64,
    /// The message.
    pub message: String,
    /// The diagnostic code, when the server gives one.
    pub code: Option<String>,
}

impl Diagnostic {
    /// Parses an LSP `Diagnostic` object (missing severity = error).
    #[must_use]
    pub fn from_lsp(value: &Value) -> Option<Self> {
        let start = value.get("range")?.get("start")?;
        let code = match value.get("code") {
            Some(Value::String(code)) => Some(code.clone()),
            Some(Value::Number(code)) => Some(code.to_string()),
            _ => None,
        };
        Some(Self {
            severity: value
                .get("severity")
                .and_then(Value::as_u64)
                .and_then(|s| u8::try_from(s).ok())
                .unwrap_or(1),
            line: start.get("line")?.as_u64()? + 1,
            column: start.get("character")?.as_u64()? + 1,
            message: value.get("message")?.as_str()?.trim().to_owned(),
            code,
        })
    }

    /// `<SEVERITY> [<line>:<col>] <message> [<code>] (<server>)`.
    #[must_use]
    pub fn render(&self, server: &str) -> String {
        let severity = match self.severity {
            1 => "ERROR",
            2 => "WARNING",
            3 => "INFO",
            _ => "HINT",
        };
        let code = self
            .code
            .as_ref()
            .map(|c| format!(" [{c}]"))
            .unwrap_or_default();
        format!(
            "{severity} [{}:{}] {}{code} ({server})",
            self.line, self.column, self.message
        )
    }

    fn identity(&self) -> (u8, &str, Option<&str>) {
        (self.severity, self.message.as_str(), self.code.as_deref())
    }
}

/// The diagnostics of a `publishDiagnostics` params object.
#[must_use]
pub fn parse_list(params: &Value) -> Vec<Diagnostic> {
    params
        .get("diagnostics")
        .or_else(|| params.get("items"))
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Diagnostic::from_lsp).collect())
        .unwrap_or_default()
}

/// What `after` has that `before` did not: compared as multisets of
/// (severity, message, code), so moved diagnostics are not new.
#[must_use]
pub fn introduced(before: &[Diagnostic], after: &[Diagnostic]) -> Vec<Diagnostic> {
    let mut seen: HashMap<(u8, &str, Option<&str>), usize> = HashMap::new();
    for diagnostic in before {
        *seen.entry(diagnostic.identity()).or_default() += 1;
    }
    after
        .iter()
        .filter(|diagnostic| match seen.get_mut(&diagnostic.identity()) {
            Some(count) if *count > 0 => {
                *count -= 1;
                false
            }
            _ => true,
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn diagnostic(line: u64, message: &str) -> Value {
        json!({"range": {"start": {"line": line, "character": 4}, "end": {"line": line, "character": 7}},
               "severity": 1, "message": message, "code": "reportUndefinedVariable"})
    }

    #[test]
    fn only_new_diagnostics_are_introduced_even_when_old_ones_moved() {
        let before = parse_list(&json!({"diagnostics": [diagnostic(3, "\"x\" is not defined")]}));
        let after = parse_list(&json!({"diagnostics": [
            diagnostic(5, "\"x\" is not defined"),
            diagnostic(9, "\"foo\" is not defined"),
        ]}));
        let new = introduced(&before, &after);
        assert_eq!(new.len(), 1);
        assert_eq!(
            new[0].render("pyright"),
            "ERROR [10:5] \"foo\" is not defined [reportUndefinedVariable] (pyright)"
        );
    }

    #[test]
    fn a_diagnostic_without_code_or_severity_is_an_error_without_brackets() {
        let parsed = Diagnostic::from_lsp(&json!({
            "range": {"start": {"line": 0, "character": 0}}, "message": "bad"
        }))
        .unwrap();
        assert_eq!(parsed.render("x"), "ERROR [1:1] bad (x)");
    }

    #[tokio::test]
    async fn messages_round_trip_through_the_framing() {
        let mut buffer = Vec::new();
        write_message(&mut buffer, &json!({"jsonrpc": "2.0", "id": 1}))
            .await
            .unwrap();
        write_message(&mut buffer, &json!({"method": "é"}))
            .await
            .unwrap();
        let mut reader = tokio::io::BufReader::new(buffer.as_slice());
        assert_eq!(read_message(&mut reader).await.unwrap().unwrap()["id"], 1);
        assert_eq!(
            read_message(&mut reader).await.unwrap().unwrap()["method"],
            "é"
        );
        assert!(read_message(&mut reader).await.unwrap().is_none());
    }
}
