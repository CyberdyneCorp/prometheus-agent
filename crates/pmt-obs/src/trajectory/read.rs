//! Reading trajectories (spec: observability, Grammar versioning and
//! forward compatibility): a newer minor version is read (unknown event
//! types are preserved), a newer major is refused with exit 76.

use std::io::BufRead;
use std::path::Path;

use pmt_core::error::{Error, codes};

use super::{Line, files, version};

/// Parses one line and checks its grammar version.
///
/// # Errors
///
/// `trajectory.malformed` for a line that is not a grammar line;
/// `trajectory.grammar_too_new` for a newer major version.
pub fn parse_line(text: &str, number: usize) -> Result<Line, Error> {
    let line: Line = serde_json::from_str(text).map_err(|err| {
        Error::new(
            &codes::TRAJECTORY_MALFORMED,
            format!("trajectory line {number}: {err}"),
        )
    })?;
    check_version(&line.v, number)?;
    Ok(line)
}

fn check_version(v: &str, number: usize) -> Result<(), Error> {
    let major = v
        .split_once('.')
        .and_then(|(major, minor)| minor.parse::<u32>().ok().and(major.parse::<u32>().ok()))
        .ok_or_else(|| {
            Error::new(
                &codes::TRAJECTORY_MALFORMED,
                format!("trajectory line {number}: invalid grammar version '{v}'"),
            )
        })?;
    if major > crate::GRAMMAR_VERSION {
        return Err(Error::new(
            &codes::TRAJECTORY_GRAMMAR_TOO_NEW,
            format!(
                "trajectory grammar {v} is newer than supported {}; run 'prometheus update'",
                version()
            ),
        ));
    }
    Ok(())
}

/// Every line of a trajectory. An unterminated last line that does not
/// parse is the tail of an interrupted write and is ignored.
///
/// # Errors
///
/// As [`parse_line`]; `trajectory.io` on a read failure.
pub fn read(mut reader: impl BufRead) -> Result<Vec<Line>, Error> {
    let mut lines = Vec::new();
    let mut buffer = String::new();
    for number in 1.. {
        buffer.clear();
        let read = reader
            .read_line(&mut buffer)
            .map_err(|err| io_error(&err))?;
        if read == 0 {
            break;
        }
        let complete = buffer.ends_with('\n');
        let text = buffer.trim_end_matches(['\n', '\r']);
        if text.is_empty() {
            continue;
        }
        match parse_line(text, number) {
            Ok(line) => lines.push(line),
            Err(err) if !complete && err.code().code == codes::TRAJECTORY_MALFORMED.code => break,
            Err(err) => return Err(err),
        }
    }
    Ok(lines)
}

/// Reads a `.jsonl` or `.jsonl.zst` trajectory file.
///
/// # Errors
///
/// As [`read`]; `trajectory.io` when the file cannot be opened.
pub fn read_file(path: &Path) -> Result<Vec<Line>, Error> {
    let reader = files::open(path).map_err(|err| {
        Error::new(
            &codes::TRAJECTORY_IO,
            format!("cannot read {}: {err}", path.display()),
        )
    })?;
    read(reader)
}

fn io_error(err: &std::io::Error) -> Error {
    Error::new(
        &codes::TRAJECTORY_IO,
        format!("cannot read the trajectory: {err}"),
    )
}

#[cfg(test)]
mod tests {
    use super::read;

    const LINE: &str = r#"{"v":"1.0","ts":"t","seq":1,"session_id":"s","root_session_id":"s","turn_id":null,"span_id":"a","parent_span_id":null,"type":"user.message","payload":{"text":"hi"}}"#;

    #[test]
    fn a_newer_major_is_refused_with_the_update_hint() {
        let newer = LINE.replace("\"1.0\"", "\"2.0\"");
        let err = read(newer.as_bytes()).unwrap_err();
        assert_eq!(
            err.message(),
            "trajectory grammar 2.0 is newer than supported 1.0; run 'prometheus update'"
        );
        assert_eq!(err.exit_code(), 76);
    }

    #[test]
    fn a_newer_minor_is_read() {
        let newer = LINE.replace("\"1.0\"", "\"1.3\"");
        assert_eq!(read(newer.as_bytes()).unwrap().len(), 1);
    }

    #[test]
    fn a_torn_last_line_is_ignored_but_a_bad_middle_line_is_not() {
        let torn = format!("{LINE}\n{{\"v\":\"1.0\",\"ts");
        assert_eq!(read(torn.as_bytes()).unwrap().len(), 1);
        let bad = format!("{{oops}}\n{LINE}\n");
        assert_eq!(
            read(bad.as_bytes()).unwrap_err().code().code,
            "trajectory.malformed"
        );
    }
}
