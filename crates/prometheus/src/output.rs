//! The machine-readable output contract (spec: workspace-architecture,
//! Machine-readable output contract; cli, Machine-readable output).
//!
//! With `--json` a command writes exactly one JSON document to stdout and
//! nothing else; progress, warnings and logs go to stderr. Listing and
//! status commands wrap their document with a top-level `schema` field
//! naming a versioned shape (`prometheus.<noun>.v<major>`) registered in
//! [`JSON_SHAPES`], which the generated CLI reference lists. A failing
//! command prints the error document to stderr.

use std::io::{self, Write};

use pmt_core::Error;
use pmt_core::error::codes;
use serde::Serialize;

/// A versioned JSON document shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonShape {
    /// `prometheus.<noun>.v<major>`; a breaking change bumps the major.
    pub schema: &'static str,
    /// The command that emits it, for example `sessions list`.
    pub command: &'static str,
}

/// Every shape a listing or status command emits. A command adds its shape
/// here when it gains `--json`.
pub const JSON_SHAPES: &[JsonShape] = &[
    JsonShape {
        schema: "prometheus.grants.v1",
        command: "grants list",
    },
    JsonShape {
        schema: "prometheus.usage.v1",
        command: "usage",
    },
    JsonShape {
        schema: "prometheus.config.v1",
        command: "config show",
    },
    JsonShape {
        schema: "prometheus.config_value.v1",
        command: "config get",
    },
    JsonShape {
        schema: "prometheus.config_explain.v1",
        command: "config explain",
    },
    JsonShape {
        schema: "prometheus.secrets.v1",
        command: "secrets list",
    },
    JsonShape {
        schema: "prometheus.backup.v1",
        command: "backup create",
    },
    JsonShape {
        schema: "prometheus.restore.v1",
        command: "backup restore",
    },
    JsonShape {
        schema: "prometheus.snapshots.v1",
        command: "snapshots list",
    },
    JsonShape {
        schema: "prometheus.profiles.v1",
        command: "profile list",
    },
    JsonShape {
        schema: "prometheus.profile.v1",
        command: "profile show",
    },
    JsonShape {
        schema: "prometheus.sessions.v1",
        command: "sessions list",
    },
    JsonShape {
        schema: "prometheus.session.v1",
        command: "sessions show",
    },
    JsonShape {
        schema: "prometheus.search.v1",
        command: "sessions search",
    },
    JsonShape {
        schema: "prometheus.session_tree.v1",
        command: "sessions tree",
    },
    JsonShape {
        schema: "prometheus.session_stats.v1",
        command: "sessions stats",
    },
    JsonShape {
        schema: "prometheus.prune.v1",
        command: "sessions prune",
    },
    JsonShape {
        schema: "prometheus.logs.v1",
        command: "logs",
    },
    JsonShape {
        schema: "prometheus.log_components.v1",
        command: "logs list",
    },
    JsonShape {
        schema: "prometheus.trajectory.v1",
        command: "trajectory show",
    },
];

/// Writes `value` as one compact JSON document and a newline.
///
/// # Errors
///
/// Writing to `out` failed.
pub fn write_json(out: &mut impl Write, value: &impl Serialize) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    out.write_all(&bytes)?;
    out.flush()
}

/// Writes a listing/status document `{"schema": <shape>, ...value}`;
/// `value` must serialize as an object (a struct or map).
///
/// # Errors
///
/// Writing failed, or `value` is not an object.
pub fn write_document(
    out: &mut impl Write,
    shape: &JsonShape,
    value: &impl Serialize,
) -> io::Result<()> {
    #[derive(Serialize)]
    struct Document<'a, T> {
        schema: &'static str,
        #[serde(flatten)]
        body: &'a T,
    }
    let document = Document {
        schema: shape.schema,
        body: value,
    };
    // Serialize fully before writing so a refused body writes nothing.
    let mut bytes = serde_json::to_vec(&document).map_err(io::Error::other)?;
    bytes.push(b'\n');
    out.write_all(&bytes)?;
    out.flush()
}

/// Prints a failure to stderr: the JSON error document with `--json`,
/// otherwise `error: <message>` and an optional `hint: <hint>` line.
pub fn print_error(err: &Error, json: bool) {
    let _ = io::stderr().write_all(render_error(err, json).as_bytes());
}

/// The error as printed: the `--json` document or `error:`/`hint:` lines,
/// with every known secret value and credential-shaped token redacted
/// (spec: security-boundaries, Known-secret redaction everywhere).
#[must_use]
pub fn render_error(err: &Error, json: bool) -> String {
    let text = if json {
        format!("{}\n", err.to_json())
    } else {
        match err.hint() {
            Some(hint) => format!("error: {err}\nhint: {hint}\n"),
            None => format!("error: {err}\n"),
        }
    };
    match pmt_obs::logs::handle() {
        Some(logs) => logs.redact(&text),
        None => pmt_core::secret::Redactor::default().redact_all(&text),
    }
}

/// Prints the listing/status document of `command` (its shape must be in
/// [`JSON_SHAPES`]) on stdout; returns exit 0.
///
/// # Errors
///
/// `output.io` when stdout cannot be written.
///
/// # Panics
///
/// When `command` has no registered shape (a programming error).
pub fn document(command: &str, body: &impl Serialize) -> Result<u8, Error> {
    let shape = JSON_SHAPES
        .iter()
        .find(|s| s.command == command)
        .expect("every JSON shape is registered");
    write_document(&mut io::stdout().lock(), shape, body).map_err(|e| stdout_error(&e))?;
    Ok(0)
}

/// Prints one plain JSON value on stdout; returns exit 0.
///
/// # Errors
///
/// `output.io`.
pub fn json(value: &impl Serialize) -> Result<u8, Error> {
    write_json(&mut io::stdout().lock(), value).map_err(|e| stdout_error(&e))?;
    Ok(0)
}

/// Prints `value` and a newline on stdout; returns exit 0.
///
/// # Errors
///
/// `output.io`.
pub fn print(value: &impl std::fmt::Display) -> Result<u8, Error> {
    writeln!(io::stdout().lock(), "{value}").map_err(|e| stdout_error(&e))?;
    Ok(0)
}

/// Prints `text` as is on stdout; returns exit 0.
///
/// # Errors
///
/// `output.io`.
pub fn print_raw(text: &str) -> Result<u8, Error> {
    io::stdout()
        .lock()
        .write_all(text.as_bytes())
        .map_err(|e| stdout_error(&e))?;
    Ok(0)
}

fn stdout_error(e: &io::Error) -> Error {
    Error::new(&codes::OUTPUT_IO, format!("cannot write to stdout: {e}"))
}

#[cfg(test)]
mod tests {
    use serde::Serialize;

    use super::{JSON_SHAPES, JsonShape, write_document};

    #[derive(Serialize)]
    struct Listing {
        items: Vec<u32>,
    }

    #[test]
    fn document_leads_with_its_schema_on_one_line() {
        let shape = JsonShape {
            schema: "prometheus.things.v1",
            command: "things list",
        };
        let mut out = Vec::new();
        write_document(&mut out, &shape, &Listing { items: vec![1, 2] }).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"schema\":\"prometheus.things.v1\",\"items\":[1,2]}\n"
        );
    }

    #[test]
    fn non_object_bodies_are_refused() {
        let shape = JsonShape {
            schema: "prometheus.things.v1",
            command: "things list",
        };
        assert!(write_document(&mut Vec::new(), &shape, &[1, 2]).is_err());
    }

    #[test]
    fn registered_shapes_are_versioned_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for shape in JSON_SHAPES {
            let (noun, major) = shape
                .schema
                .strip_prefix("prometheus.")
                .and_then(|rest| rest.rsplit_once(".v"))
                .unwrap_or_else(|| panic!("{} is not prometheus.<noun>.v<major>", shape.schema));
            assert!(
                !noun.is_empty() && major.parse::<u32>().is_ok(),
                "{}",
                shape.schema
            );
            assert!(seen.insert(shape.schema), "duplicate {}", shape.schema);
        }
    }
}
