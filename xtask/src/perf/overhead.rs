//! The observability overhead gate (spec: observability, Observability
//! overhead budget): the bundled smoke suite's wall time with trajectory
//! recording and logging on (the defaults) may exceed the same suite with
//! recording off by at most 3%, and with OpenTelemetry export on (to a
//! local collector) by at most 5%.
//!
//! The three variants run interleaved [`RUNS`] times each through the
//! release binary's hidden `smoke-suite --extra-config <file>`, and the
//! fastest run of each counts: the minimum is the measurement least
//! disturbed by the rest of the machine. A difference under
//! [`NOISE_FLOOR_SECS`] never counts. The `interrupt` task is left out of
//! every variant: it finds its running turn through the trajectory, which
//! the reference variant does not record.

use std::io::{BufReader, Write as _};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

/// Runs of each variant.
pub(crate) const RUNS: usize = 5;
/// Allowed slowdown with recording and logging on.
pub(crate) const RECORDING_BUDGET: f64 = 1.03;
/// Allowed slowdown with OpenTelemetry export on as well.
pub(crate) const EXPORT_BUDGET: f64 = 1.05;
/// Differences below this many seconds are noise.
pub(crate) const NOISE_FLOOR_SECS: f64 = 0.05;

/// Recording and logging off: the reference.
const OFF: &str =
    "\n[observability.trajectories]\nenabled = false\n\n[logging]\nlevel = \"error\"\n";

/// The fastest smoke-suite wall time of each variant, in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Overhead {
    /// Recording and logging off.
    pub(crate) off: f64,
    /// The defaults: trajectories recorded, logs written.
    pub(crate) recording: f64,
    /// The defaults plus OTLP export to a local collector.
    pub(crate) export: f64,
}

/// The budgets `overhead` breaks, one line each.
pub(crate) fn evaluate(overhead: &Overhead) -> Vec<String> {
    let check = |name: &str, measured: f64, budget: f64| {
        let over = measured - overhead.off * budget;
        (over > 0.0 && measured - overhead.off > NOISE_FLOOR_SECS).then(|| {
            format!(
                "observability overhead: {name} {measured:.3}s is {:.1}% above {:.3}s with recording off (budget {:.0}%)",
                (measured / overhead.off - 1.0) * 100.0,
                overhead.off,
                (budget - 1.0) * 100.0
            )
        })
    };
    [
        check(
            "smoke suite with recording and logging",
            overhead.recording,
            RECORDING_BUDGET,
        ),
        check(
            "smoke suite with OTLP export",
            overhead.export,
            EXPORT_BUDGET,
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Measures the three variants with `binary`, in `scratch`.
pub(crate) fn measure(binary: &Path, scratch: &Path) -> Result<Overhead, String> {
    let _ = std::fs::remove_dir_all(scratch);
    std::fs::create_dir_all(scratch).map_err(|e| format!("{}: {e}", scratch.display()))?;
    let collector = collector()?;
    let export = format!("\n[observability.otel]\nenabled = true\nendpoint = \"{collector}\"\n");
    let variants = [
        ("off", OFF.to_owned()),
        ("recording", String::new()),
        ("export", export),
    ];
    let mut best = [f64::MAX; 3];
    for run in 0..RUNS {
        for (index, (name, extra)) in variants.iter().enumerate() {
            let config = scratch.join(format!("{name}.toml"));
            std::fs::write(&config, extra).map_err(|e| format!("{}: {e}", config.display()))?;
            let output = scratch.join(format!("{name}-{run}"));
            best[index] = best[index].min(time_suite(binary, &config, &output)?);
        }
    }
    Ok(Overhead {
        off: best[0],
        recording: best[1],
        export: best[2],
    })
}

/// One smoke-suite run's wall time.
fn time_suite(binary: &Path, config: &Path, output: &Path) -> Result<f64, String> {
    let started = Instant::now();
    let out = Command::new(binary)
        .args(["smoke-suite", "--skip", "interrupt", "--extra-config"])
        .arg(config)
        .arg("--output")
        .arg(output)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{}: {e}", binary.display()))?;
    let took = started.elapsed().as_secs_f64();
    if !out.status.success() {
        return Err(format!(
            "the smoke suite failed with {}: {}",
            config.display(),
            String::from_utf8_lossy(&out.stdout)
        ));
    }
    Ok(took)
}

/// A local OTLP/HTTP collector answering 200 to every request; its URL.
fn collector() -> Result<String, String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let url = format!(
        "http://{}",
        listener.local_addr().map_err(|e| e.to_string())?
    );
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || answer(stream));
        }
    });
    Ok(url)
}

fn answer(stream: std::net::TcpStream) {
    let Ok(read) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read);
    let mut writer = stream;
    while skip_request(&mut reader).is_some() {
        if writer
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
            .is_err()
        {
            return;
        }
    }
}

/// Reads one request (request line, headers, sized body); `None` once the
/// connection ends.
fn skip_request(reader: &mut impl std::io::BufRead) -> Option<()> {
    let mut length = 0;
    let mut first = true;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if !first && line.trim().is_empty() {
            break;
        }
        first = false;
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()
}

#[cfg(test)]
mod tests {
    use super::{Overhead, evaluate};

    /// Scenario: Overhead gate. A change that made trajectory writing
    /// synchronous on the turn path shows as a recording variant more than
    /// 3% slower than recording off, and the gate fails on it.
    #[test]
    fn recording_more_than_3_percent_slower_fails_the_gate() {
        let synchronous = Overhead {
            off: 10.0,
            recording: 10.4,
            export: 10.45,
        };
        let violations = evaluate(&synchronous);
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(
            violations[0].contains("recording and logging 10.400s is 4.0% above 10.000s"),
            "{violations:?}"
        );
        let within = Overhead {
            off: 10.0,
            recording: 10.25,
            export: 10.45,
        };
        assert!(evaluate(&within).is_empty());
        let export_heavy = Overhead {
            export: 10.6,
            ..within
        };
        assert_eq!(evaluate(&export_heavy).len(), 1);
    }

    #[test]
    fn differences_under_the_noise_floor_never_count() {
        let tiny = Overhead {
            off: 0.5,
            recording: 0.54,
            export: 0.54,
        };
        assert!(evaluate(&tiny).is_empty());
    }
}
