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
//! each task's median run. The median, not the fastest run: on a shared CI
//! runner the fastest of a few runs is whichever run got lucky, and how
//! lucky differs between variants by more than the budgets (CI saw the
//! same build pass at +0.4% and fail at +3.3%); the median of [`RUNS`]
//! ignores every disturbed run as long as most runs are not. A difference
//! under [`NOISE_FLOOR_SECS`] never counts. Two
//! tasks are left out of every variant ([`UNTIMED`]): `interrupt` finds
//! its running turn through the trajectory, which the reference variant
//! does not record, and `error_recovery` spends most of its time in the
//! retry's full-jitter backoff, a uniformly random wait of up to a second
//! (`classify::RetryPolicy`): the fastest and the median of a few runs
//! alike differ by a hundred milliseconds or more between variants,
//! several times the budgets, whatever observability costs.

use std::io::{BufReader, Write as _};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

/// Smoke tasks the gate does not time (see the module docs).
const UNTIMED: &[&str] = &["interrupt", "error_recovery"];

/// Runs of each task in each variant (odd, so the median is one run).
pub(crate) const RUNS: usize = 11;
/// Allowed slowdown with recording and logging on.
pub(crate) const RECORDING_BUDGET: f64 = 1.03;
/// Allowed slowdown with OpenTelemetry export on as well.
pub(crate) const EXPORT_BUDGET: f64 = 1.05;
/// Differences below this many seconds are noise.
pub(crate) const NOISE_FLOOR_SECS: f64 = 0.05;

/// Recording and logging off: the reference.
const OFF: &str =
    "\n[observability.trajectories]\nenabled = false\n\n[logging]\nlevel = \"error\"\n";

/// The smoke-suite wall time of each variant (sum of per-task medians), in
/// seconds.
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
    // runs[variant][task]: the task's wall time in each run of that variant.
    let mut runs = vec![vec![Vec::with_capacity(RUNS); tasks.len()]; variants.len()];
    for run in 0..RUNS {
        for (t, task) in tasks.iter().enumerate() {
            for (v, (name, _)) in variants.iter().enumerate() {
                let suite = Suite {
                    config: &scratch.join(format!("{name}.toml")),
                    output: &scratch.join(format!("{name}-{task}-{run}")),
                    task,
                    tasks,
                };
                runs[v][t].push(time_task(binary, &suite)?);
            }
        }
    }
    for (t, task) in tasks.iter().enumerate() {
        let ms = |v: usize| median(&runs[v][t]) * 1000.0;
        println!(
            "perf: overhead {task}: off {:.1} ms, recording {:.1} ms, export {:.1} ms (median of {RUNS})",
            ms(0),
            ms(1),
            ms(2)
        );
    }
    Ok(Overhead {
        off: variant_time(&runs[0]),
        recording: variant_time(&runs[1]),
        export: variant_time(&runs[2]),
    })
}

/// A variant's time: the sum of each task's median run.
fn variant_time(runs: &[Vec<f64>]) -> f64 {
    runs.iter().map(|task| median(task)).sum()
}

/// The median of `samples` (the mean of the middle two for an even count).
fn median(samples: &[f64]) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[mid]
    } else {
        f64::midpoint(sorted[mid - 1], sorted[mid])
    }
}

/// The smoke-suite tasks the gate times: every one but [`UNTIMED`].
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
        .filter(|t| !UNTIMED.contains(&t.as_str()))
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
    command.arg("smoke-suite");
    for untimed in UNTIMED {
        command.args(["--skip", untimed]);
    }
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
    use super::{Overhead, evaluate, variant_time};

    /// One disturbed run (a runner hiccup) moves neither variant: the
    /// gate compares typical runs, not the luckiest or unluckiest one.
    #[test]
    fn a_single_disturbed_run_does_not_move_a_variant() {
        let steady = vec![vec![0.20, 0.21, 0.20, 0.22, 0.21]; 3];
        let mut disturbed = steady.clone();
        disturbed[1][2] = 0.90;
        disturbed[2][0] = 0.05;
        assert!((variant_time(&steady) - variant_time(&disturbed)).abs() < 1e-9);
        assert!((variant_time(&steady) - 0.63).abs() < 1e-9);
    }

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
