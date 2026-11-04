//! The launcher's argument protocol and Windows command-line quoting.
//!
//! `<launcher> __pmt-sandbox-launch <container> <i|n> <s|-> <limits|-> -- <program> [args...]`:
//! the `AppContainer` profile name, whether the child gets the
//! `internetClient` capability, whether it may start processes, and the
//! Job Object's resource limits as `<processes>:<memory bytes>`.

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStrExt;

use crate::policy::ResourceLimits;

const USAGE: &str =
    "usage: __pmt-sandbox-launch <container> <i|n> <s|-> <limits|-> -- <program> [args...]";

/// What the launcher starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaunchSpec {
    pub(crate) container: String,
    pub(crate) internet: bool,
    pub(crate) spawn: bool,
    pub(crate) limits: Option<ResourceLimits>,
    pub(crate) program: OsString,
    pub(crate) args: Vec<OsString>,
}

impl LaunchSpec {
    /// The launcher arguments up to and including the program.
    pub(crate) fn head(
        container: &str,
        internet: bool,
        spawn: bool,
        limits: Option<ResourceLimits>,
        program: &OsStr,
    ) -> Vec<OsString> {
        let limits = limits.map_or_else(
            || "-".to_owned(),
            |l| format!("{}:{}", l.processes, l.memory_bytes),
        );
        vec![
            OsString::from(crate::LAUNCH_ARG),
            OsString::from(container),
            OsString::from(if internet { "i" } else { "n" }),
            OsString::from(if spawn { "s" } else { "-" }),
            OsString::from(limits),
            OsString::from("--"),
            program.to_os_string(),
        ]
    }

    /// Parses the arguments after the launch marker.
    pub(crate) fn parse(args: &[OsString]) -> Result<Self, String> {
        let text = |i: usize| args.get(i).and_then(|a| a.to_str());
        let (Some(container), Some(net), Some(spawn), Some(limits), Some("--")) =
            (text(0), text(1), text(2), text(3), text(4))
        else {
            return Err(USAGE.to_owned());
        };
        let limits = match limits.split_once(':') {
            None => None,
            Some((processes, memory)) => Some(ResourceLimits {
                processes: processes.parse().map_err(|_| USAGE.to_owned())?,
                memory_bytes: memory.parse().map_err(|_| USAGE.to_owned())?,
            }),
        };
        let program = args.get(5).ok_or("missing program")?.clone();
        Ok(Self {
            container: container.to_owned(),
            internet: net == "i",
            spawn: spawn == "s",
            limits,
            program,
            args: args[6..].to_vec(),
        })
    }
}

/// The NUL-terminated command line for `CreateProcessW`, quoted so the
/// child's C runtime splits it back into exactly `program` and `args`.
pub(crate) fn command_line(program: &OsStr, args: &[OsString]) -> Vec<u16> {
    let mut line = Vec::new();
    quote(program, &mut line);
    for arg in args {
        line.push(u16::from(b' '));
        quote(arg, &mut line);
    }
    line.push(0);
    line
}

fn quote(arg: &OsStr, out: &mut Vec<u16>) {
    let units: Vec<u16> = arg.encode_wide().collect();
    let special = |c: &u16| [b' ', b'\t', b'"'].iter().any(|b| *c == u16::from(*b));
    if !units.is_empty() && !units.iter().any(special) {
        out.extend(units);
        return;
    }
    let backslash = u16::from(b'\\');
    let quote_mark = u16::from(b'"');
    out.push(quote_mark);
    let mut pending = 0usize;
    for unit in units {
        if unit == backslash {
            pending += 1;
            continue;
        }
        let escapes = if unit == quote_mark {
            pending * 2 + 1
        } else {
            pending
        };
        out.extend(std::iter::repeat_n(backslash, escapes));
        out.push(unit);
        pending = 0;
    }
    out.extend(std::iter::repeat_n(backslash, pending * 2));
    out.push(quote_mark);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(args: &[&str]) -> String {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let wide = command_line(OsStr::new("prog"), &args);
        String::from_utf16(&wide[..wide.len() - 1]).unwrap()
    }

    #[test]
    fn quoting_follows_the_crt_rules() {
        assert_eq!(line(&["a", "b c", ""]), r#"prog a "b c" """#);
        assert_eq!(line(&[r#"say "hi""#]), r#"prog "say \"hi\"""#);
        assert_eq!(
            line(&[r"C:\dir with space\"]),
            r#"prog "C:\dir with space\\""#
        );
        assert_eq!(line(&[r"C:\plain\path"]), r"prog C:\plain\path");
    }

    #[test]
    fn spec_round_trips() {
        let limits = ResourceLimits {
            memory_bytes: 1 << 30,
            processes: 16,
        };
        let head = LaunchSpec::head("c1", true, false, Some(limits), OsStr::new("cmd.exe"));
        let mut args: Vec<OsString> = head[1..].to_vec();
        args.push(OsString::from("/C"));
        let spec = LaunchSpec::parse(&args).unwrap();
        assert!(spec.internet && !spec.spawn);
        assert_eq!(spec.limits, Some(limits));
        assert_eq!(spec.program, "cmd.exe");
        assert_eq!(spec.args, [OsString::from("/C")]);
    }
}
