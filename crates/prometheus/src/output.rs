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
    let text = if json {
        format!("{}\n", err.to_json())
    } else {
        match err.hint() {
            Some(hint) => format!("error: {err}\nhint: {hint}\n"),
            None => format!("error: {err}\n"),
        }
    };
    let _ = io::stderr().write_all(text.as_bytes());
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
