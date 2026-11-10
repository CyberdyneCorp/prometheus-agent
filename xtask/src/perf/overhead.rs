//! The observability overhead gate (spec: observability, Observability
//! overhead budget): the bundled smoke suite's wall time with trajectory
//! recording and logging on (the defaults) may exceed the same suite with
//! recording off by at most 3%, and with OpenTelemetry export on (to a
//! local collector) by at most 5%.
//!
//! The suite's wall time jitters by tens of percent between runs (each
//! task spawns several processes), far more than the budgets, so the
//! variants are measured task by task: every task runs alone (the hidden
//! `smoke-suite --skip` leaves the others out) in each variant, the
//! variants interleaved, [`RUNS`] times, and a variant's time is the sum of
//! each task's fastest run, the measurement least disturbed by the rest of
//! the machine. A difference under [`NOISE_FLOOR_SECS`] never counts. The
//! `interrupt` task is left out of every variant: it finds its running
//! turn through the trajectory, which the reference variant does not
//! record.

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

/// Measures the three variants of every task in `tasks` with `binary`,
/// in `scratch`.
pub(crate) fn measure(binary: &Path, scratch: &Path, tasks: &[String]) -> Result<Overhead, String> {
    let _ = std::fs::remove_dir_all(scratch);
    std::fs::create_dir_all(scratch).map_err(|e| format!("{}: {e}", scratch.display()))?;
    let collector = collector()?;
    let export = format!("\n[observability.otel]\nenabled = true\nendpoint = \"{collector}\"\n");
    let variants = [
        ("off", OFF.to_owned()),
        ("recording", String::new()),
        ("export", export),
    ];
    for (name, extra) in &variants {
        let config = scratch.join(format!("{name}.toml"));
        std::fs::write(&config, extra).map_err(|e| format!("{}: {e}", config.display()))?;
    }
    // best[variant][task]: the task's fastest run in that variant.
    let mut best = vec![vec![f64::MAX; tasks.len()]; variants.len()];
    for run in 0..RUNS {
        for (t, task) in tasks.iter().enumerate() {
            for (v, (name, _)) in variants.iter().enumerate() {
                let suite = Suite {
                    config: &scratch.join(format!("{name}.toml")),
                    output: &scratch.join(format!("{name}-{task}-{run}")),
                    task,
                    tasks,
                };
                best[v][t] = best[v][t].min(time_task(binary, &suite)?);
            }
        }
    }
    let total = |v: usize| best[v].iter().sum();
    Ok(Overhead {
        off: total(0),
        recording: total(1),
        export: total(2),
    })
}

/// The smoke-suite tasks the gate times: every one but `interrupt`.
///
/// # Errors
///
/// The suite's task directory cannot be read.
pub(crate) fn tasks(root: &Path) -> Result<Vec<String>, String> {
    let dir = root.join("crates/prometheus/evals/smoke/tasks");
    let mut tasks: Vec<String> = std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .flatten()
        .filter(|e| e.path().join("task.toml").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|t| t != "interrupt")
        .collect();
    tasks.sort();
    Ok(tasks)
}

/// One task of the suite run alone with one variant's config.
struct Suite<'a> {
    config: &'a Path,
    output: &'a Path,
    task: &'a str,
    tasks: &'a [String],
}

/// The wall time of `suite`'s task run alone.
fn time_task(binary: &Path, suite: &Suite<'_>) -> Result<f64, String> {
    let mut command = Command::new(binary);
    command.args(["smoke-suite", "--skip", "interrupt"]);
    for other in suite.tasks.iter().filter(|t| *t != suite.task) {
        command.args(["--skip", other]);
    }
    command
        .arg("--extra-config")
        .arg(suite.config)
        .arg("--output")
        .arg(suite.output)
        .stdin(Stdio::null());
    let started = Instant::now();
    let out = command
        .output()
        .map_err(|e| format!("{}: {e}", binary.display()))?;
    let took = started.elapsed().as_secs_f64();
    if !out.status.success() {
        return Err(format!(
            "the smoke suite's {} task failed with {}: {}",
            suite.task,
            suite.config.display(),
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
