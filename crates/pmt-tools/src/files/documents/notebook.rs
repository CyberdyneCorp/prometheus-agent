//! Jupyter notebooks as Markdown: Markdown cells as they are, code cells
//! fenced, outputs after them. An output longer than [`MAX_OUTPUT_CHARS`]
//! is cut with a marker naming a command that prints all of it.

use std::fmt::Write as _;

use serde_json::Value;

/// Characters of one cell output shown.
pub(super) const MAX_OUTPUT_CHARS: usize = 20_000;

pub(super) fn render(shown: &str, bytes: &[u8]) -> Result<String, String> {
    let notebook: Value = serde_json::from_slice(bytes)
        .map_err(|e| format!("'{shown}' is not a valid notebook: {e}"))?;
    let cells = notebook
        .get("cells")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("'{shown}' is not a valid notebook: no cells"))?;
    let language = notebook
        .pointer("/metadata/kernelspec/language")
        .or_else(|| notebook.pointer("/metadata/language_info/name"))
        .and_then(Value::as_str)
        .unwrap_or("python");
    let mut text = String::new();
    for (index, cell) in cells.iter().enumerate() {
        let source = joined(cell.get("source"));
        match cell.get("cell_type").and_then(Value::as_str) {
            Some("code") => {
                let _ = write!(
                    text,
                    "## Cell {index} (code)\n\n```{language}\n{}\n```\n\n",
                    source.trim_end()
                );
                outputs(&mut text, shown, index, cell);
            }
            Some(kind) => {
                let _ = write!(
                    text,
                    "## Cell {index} ({kind})\n\n{}\n\n",
                    source.trim_end()
                );
            }
            None => {}
        }
    }
    Ok(text)
}

/// A notebook string field: a string or a list of lines.
fn joined(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(lines)) => lines.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    }
}

fn outputs(text: &mut String, shown: &str, cell: usize, value: &Value) {
    let Some(outputs) = value.get("outputs").and_then(Value::as_array) else {
        return;
    };
    for (index, output) in outputs.iter().enumerate() {
        let body = match output.get("output_type").and_then(Value::as_str) {
            Some("stream") => joined(output.get("text")),
            Some("error") => joined(output.get("traceback")).replace("\n\n", "\n"),
            _ => joined(output.pointer("/data/text~1plain")),
        };
        if body.is_empty() {
            continue;
        }
        let body = crate::terminal::shape::strip_ansi(&body);
        let count = body.chars().count();
        let _ = write!(text, "Output {index}:\n\n```\n");
        if count > MAX_OUTPUT_CHARS {
            let kept: String = body.chars().take(MAX_OUTPUT_CHARS).collect();
            let _ = write!(
                text,
                "{kept}\n[output truncated: {count} characters; print all of it with: jq -r '.cells[{cell}].outputs[{index}] | (.text // .traceback // .data[\"text/plain\"]) | if type == \"array\" then join(\"\") else . end' {shown}]\n"
            );
        } else {
            let _ = writeln!(text, "{}", body.trim_end());
        }
        text.push_str("```\n\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_and_long_outputs_are_rendered() {
        let long = "x".repeat(MAX_OUTPUT_CHARS + 5);
        let notebook = serde_json::json!({
            "metadata": {"kernelspec": {"language": "python"}},
            "cells": [
                {"cell_type": "markdown", "source": ["# Title\n", "intro"]},
                {"cell_type": "code", "source": "print(1)", "outputs": [
                    {"output_type": "stream", "text": ["1\n"]},
                    {"output_type": "execute_result", "data": {"text/plain": long}}
                ]}
            ]
        });
        let text = render("nb.ipynb", notebook.to_string().as_bytes()).unwrap();
        assert!(
            text.contains("## Cell 0 (markdown)\n\n# Title\nintro"),
            "{text}"
        );
        assert!(text.contains("```python\nprint(1)\n```"));
        assert!(text.contains("Output 0:\n\n```\n1\n```"));
        assert!(text.contains(&format!(
            "[output truncated: {} characters",
            MAX_OUTPUT_CHARS + 5
        )));
        assert!(text.contains(".cells[1].outputs[1]"));
        assert!(text.contains(" nb.ipynb]"));
    }
}
