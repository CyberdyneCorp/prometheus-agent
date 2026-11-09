//! The launcher's argument protocol and Windows command-line quoting.
//!
//! `<launcher> __pmt-sandbox-launch <container> <i|n|p<port>> <s|-> <limits|-> <handles|-> -- <program> [args...]`:
//! the `AppContainer` profile name, the child's network (`i`: the
//! `internetClient` capability, `n`: none, `p<port>`: only through the
//! egress proxy on that loopback port, see [`super::relay`]), whether it
//! may start processes, the Job Object's resource limits as
//! `<processes>:<memory bytes>`, and the handle values (decimal, comma
//! separated, prefixed `h`) the child inherits beside its standard
//! handles: the launcher inherited them from its caller under the same
//! values, so the caller can name them on the child's command line (a
//! confined browser's DevTools pipe, `--remote-debugging-io-pipes`).

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStrExt;

use crate::policy::ResourceLimits;

const USAGE: &str = "usage: __pmt-sandbox-launch <container> <i|n|p<port>> <s|-> <limits|-> <handles|-> -- <program> [args...]";

/// The network a confined child gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Net {
    /// No network.
    None,
    /// Public addresses (`internetClient`).
    Internet,
    /// Only the egress proxy on this loopback port, through a relay.
    Proxy(u16),
}

impl Net {
    fn token(self) -> String {
        match self {
            Self::None => "n".to_owned(),
            Self::Internet => "i".to_owned(),
            Self::Proxy(port) => format!("p{port}"),
        }
    }

    fn parse(token: &str) -> Option<Self> {
        match token {
            "n" => Some(Self::None),
            "i" => Some(Self::Internet),
            _ => token.strip_prefix('p')?.parse().ok().map(Self::Proxy),
        }
    }
}

/// What the launcher starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaunchSpec {
    pub(crate) container: String,
    pub(crate) net: Net,
    pub(crate) spawn: bool,
    pub(crate) limits: Option<ResourceLimits>,
    /// Handle values passed on to the child.
    pub(crate) handles: Vec<usize>,
    pub(crate) program: OsString,
    pub(crate) args: Vec<OsString>,
}

impl LaunchSpec {
    /// The launcher arguments up to and including the program (`args`
    /// are added by the caller).
    pub(crate) fn head(&self) -> Vec<OsString> {
        let limits = self.limits.map_or_else(
            || "-".to_owned(),
            |l| format!("{}:{}", l.processes, l.memory_bytes),
        );
        let handles = if self.handles.is_empty() {
            "-".to_owned()
        } else {
            let values: Vec<String> = self.handles.iter().map(ToString::to_string).collect();
            format!("h{}", values.join(","))
        };
        vec![
            OsString::from(crate::LAUNCH_ARG),
            OsString::from(&self.container),
            OsString::from(self.net.token()),
            OsString::from(if self.spawn { "s" } else { "-" }),
            OsString::from(limits),
            OsString::from(handles),
            OsString::from("--"),
            self.program.clone(),
        ]
    }

    /// Parses the arguments after the launch marker.
    pub(crate) fn parse(args: &[OsString]) -> Result<Self, String> {
        let text = |i: usize| args.get(i).and_then(|a| a.to_str());
        let (Some(container), Some(net), Some(spawn), Some(limits), Some(handles), Some("--")) =
            (text(0), text(1), text(2), text(3), text(4), text(5))
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
        let handles = match handles.strip_prefix('h') {
            None => Vec::new(),
            Some(list) => list
                .split(',')
                .map(|value| value.parse().map_err(|_| USAGE.to_owned()))
                .collect::<Result<_, _>>()?,
        };
        let net = Net::parse(net).ok_or_else(|| USAGE.to_owned())?;
        let program = args.get(6).ok_or("missing program")?.clone();
        Ok(Self {
            container: container.to_owned(),
            net,
            spawn: spawn == "s",
            limits,
            handles,
            program,
            args: args[7..].to_vec(),
        })
    }
}

/// `value` with every mention of the proxy's loopback address
/// (`127.0.0.1:<proxy>` exactly) pointed at the relay's port instead.
pub(crate) fn to_relay(value: OsString, proxy: u16, relay: u16) -> OsString {
    let from = format!("127.0.0.1:{proxy}");
    match value.to_str() {
        Some(text) if names_address(text, &from) => {
            OsString::from(text.replace(&from, &format!("127.0.0.1:{relay}")))
        }
        _ => value,
    }
}

/// The child's environment block (`CREATE_UNICODE_ENVIRONMENT`): `vars`,
/// sorted by name as Windows expects, with every value naming the proxy's
/// loopback address (`127.0.0.1:<proxy>` exactly) rewritten to the relay's
/// port, so `HTTP_PROXY` and friends lead the child to its relay.
pub(crate) fn environment(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
    proxy: u16,
    relay: u16,
) -> Vec<u16> {
    let mut vars: Vec<(OsString, OsString)> = vars
        .into_iter()
        .map(|(name, value)| (name, to_relay(value, proxy, relay)))
        .collect();
    vars.sort_by_key(|(name, _)| name.to_string_lossy().to_uppercase());
    let mut block = Vec::new();
    for (name, value) in vars {
        block.extend(name.encode_wide());
        block.push(u16::from(b'='));
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

/// Whether `text` holds `address` not followed by another port digit.
fn names_address(text: &str, address: &str) -> bool {
    text.match_indices(address).any(|(at, _)| {
        !text[at + address.len()..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
    })
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

    fn spec(net: Net, handles: Vec<usize>) -> LaunchSpec {
        LaunchSpec {
            container: "c1".to_owned(),
            net,
            spawn: false,
            limits: Some(ResourceLimits {
                memory_bytes: 1 << 30,
                processes: 16,
            }),
            handles,
            program: OsString::from("cmd.exe"),
            args: Vec::new(),
        }
    }

    #[test]
    fn spec_round_trips() {
        for (net, handles) in [
            (Net::Internet, vec![]),
            (Net::None, vec![412]),
            (Net::Proxy(8443), vec![412, 1060]),
        ] {
            let sent = spec(net, handles);
            let mut args: Vec<OsString> = sent.head()[1..].to_vec();
            args.push(OsString::from("/C"));
            let parsed = LaunchSpec::parse(&args).unwrap();
            assert_eq!(parsed.args, [OsString::from("/C")]);
            assert_eq!(
                LaunchSpec {
                    args: Vec::new(),
                    ..parsed
                },
                sent
            );
        }
    }

    #[test]
    fn arguments_naming_the_proxy_point_at_the_relay() {
        let arg = |text: &str| to_relay(OsString::from(text), 9000, 4100);
        assert_eq!(
            arg("--proxy-server=http://127.0.0.1:9000"),
            "--proxy-server=http://127.0.0.1:4100"
        );
        assert_eq!(arg("http://127.0.0.1:90001"), "http://127.0.0.1:90001");
    }

    #[test]
    fn variables_naming_the_proxy_point_at_the_relay() {
        let vars = [
            ("HTTPS_PROXY", "http://127.0.0.1:9000"),
            ("Path", r"C:\Windows"),
            ("OTHER", "http://127.0.0.1:90001"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let block = environment(vars, 9000, 4100);
        let text = String::from_utf16(&block).unwrap();
        assert_eq!(
            text,
            "HTTPS_PROXY=http://127.0.0.1:4100\0OTHER=http://127.0.0.1:90001\0Path=C:\\Windows\0\0"
        );
    }
}
