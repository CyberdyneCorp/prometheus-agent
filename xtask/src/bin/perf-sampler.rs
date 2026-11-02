//! `perf-sampler BINARY RUNS [chat HOME]`: runs `BINARY --version` (or,
//! with `chat HOME`, starts `BINARY --home HOME chat --line --provider mock`
//! in a pseudo-terminal until its prompt appears) and prints one
//! `startup_ms <ms>` line per timed run, then `peak_rss_bytes <n|none>`.
//! The chat mode then also runs `chat -z` with the mock provider, so the
//! peak covers the one-shot run the memory budget names.
//!
//! This is a separate, std-and-nix-only binary rather than an xtask
//! subcommand because peak RSS is read with `getrusage(RUSAGE_CHILDREN)`,
//! and on Linux a child inherits a floor from the process that spawned it:
//! std spawns through `posix_spawn`, which glibc implements as
//! `clone(CLONE_VM | CLONE_VFORK)`, and at `execve` the kernel folds the
//! shared (parent's) memory high-water mark into the child's `maxrss`. A
//! small spawner keeps that floor small, and [`attributable`] discards a
//! peak that does not rise above it. It also keeps `cargo build`, which the
//! `perf` parent runs, out of the children's counter.

use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::Instant;

/// Runs excluded from the samples: the first launch of a fresh binary pays
/// for page-cache misses and, on macOS, the code-signature check.
const WARMUP_RUNS: usize = 3;

/// One-shot chat runs whose peak RSS is measured in chat mode.
const ONE_SHOT_RUNS: usize = 3;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (binary, runs, home) = match args.as_slice() {
        [binary, runs] => (binary, runs, None),
        [binary, runs, mode, home] if mode == "chat" => (binary, runs, Some(Path::new(home))),
        _ => {
            eprintln!("usage: perf-sampler BINARY RUNS [chat HOME]");
            return ExitCode::FAILURE;
        }
    };
    let Ok(runs) = runs.parse::<usize>() else {
        eprintln!("perf-sampler: RUNS must be a number, got {runs}");
        return ExitCode::FAILURE;
    };
    match sample(Path::new(binary), runs, home) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("perf-sampler: {message}");
            ExitCode::FAILURE
        }
    }
}

fn sample(binary: &Path, runs: usize, chat_home: Option<&Path>) -> Result<(), String> {
    let run_once = |binary: &Path| match chat_home {
        Some(home) => chat::prompt_once(binary, home),
        None => version_once(binary),
    };
    for _ in 0..WARMUP_RUNS {
        run_once(binary)?;
    }
    for _ in 0..runs {
        println!("startup_ms {}", run_once(binary)?);
    }
    if let Some(home) = chat_home {
        for _ in 0..ONE_SHOT_RUNS {
            chat::one_shot(binary, home)?;
        }
    }
    // Read after the runs: the floor only grows, so the final value bounds
    // the floor every run inherited.
    match attributable(children_peak_rss(), inherited_floor()) {
        Some(bytes) => println!("peak_rss_bytes {bytes}"),
        None => println!("peak_rss_bytes none"),
    }
    Ok(())
}

fn version_once(binary: &Path) -> Result<f64, String> {
    let start = Instant::now();
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .map_err(|e| format!("{}: {e}", binary.display()))?;
    let elapsed = start.elapsed();
    if !output.status.success() || !output.stdout.starts_with(b"prometheus ") {
        return Err(format!(
            "{} --version failed ({}): {}",
            binary.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(elapsed.as_secs_f64() * 1_000.0)
}

/// `prometheus chat` with the mock provider (spec: Startup time budget;
/// Memory and size budgets).
mod chat {
    use std::path::Path;
    use std::process::{Command, Stdio};

    /// The line interface's idle prompt.
    #[cfg(unix)]
    const PROMPT: &[u8] = "› ".as_bytes();

    /// Starts an interactive chat in a pseudo-terminal and returns the
    /// milliseconds until its prompt is shown (and so accepts keystrokes).
    #[cfg(unix)]
    pub(super) fn prompt_once(binary: &Path, home: &Path) -> Result<f64, String> {
        use std::io::Read as _;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let size = nix::pty::Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = nix::pty::openpty(Some(&size), None).map_err(|e| format!("openpty: {e}"))?;
        let tty = |fd: &std::os::fd::OwnedFd| {
            fd.try_clone()
                .map(Stdio::from)
                .map_err(|e| format!("pty: {e}"))
        };
        let start = Instant::now();
        let mut child = Command::new(binary)
            .arg("--home")
            .arg(home)
            // `--line`: the budget is the in-process prompt. The TUI (the
            // default interface since M2) is a client of the daemon, whose
            // start is the separate daemon readiness budget, and it would
            // leave an autostarted daemon running in HOME.
            .args(["chat", "--line", "--provider", "mock"])
            .env("TERM", "xterm")
            .stdin(tty(&pty.slave)?)
            .stdout(tty(&pty.slave)?)
            .stderr(tty(&pty.slave)?)
            .spawn()
            .map_err(|e| format!("{}: {e}", binary.display()))?;
        drop(pty.slave);
        let (ready, shown) = mpsc::channel();
        let mut master = std::fs::File::from(pty.master);
        std::thread::spawn(move || {
            let (mut seen, mut buf) = (Vec::new(), [0u8; 4096]);
            while let Ok(read @ 1..) = master.read(&mut buf) {
                seen.extend_from_slice(&buf[..read]);
                if seen.windows(PROMPT.len()).any(|w| w == PROMPT) {
                    let _ = ready.send(());
                    return;
                }
            }
        });
        let outcome = shown.recv_timeout(Duration::from_secs(10));
        let elapsed = start.elapsed();
        let _ = child.kill();
        let _ = child.wait();
        outcome.map_err(|_| "chat showed no prompt within 10 s".to_owned())?;
        Ok(elapsed.as_secs_f64() * 1_000.0)
    }

    #[cfg(not(unix))]
    pub(super) fn prompt_once(_: &Path, _: &Path) -> Result<f64, String> {
        Err("chat startup is measured on Unix only".to_owned())
    }

    /// `chat -z hi` with the mock provider.
    pub(super) fn one_shot(binary: &Path, home: &Path) -> Result<(), String> {
        let output = Command::new(binary)
            .arg("--home")
            .arg(home)
            .args(["chat", "-z", "hi", "--provider", "mock"])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("{}: {e}", binary.display()))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "chat -z failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ))
        }
    }
}

/// The children's peak, unless it does not exceed the floor every child
/// inherited from this process: then it is this process's footprint, not
/// the measured binary's, and is reported as not measurable.
fn attributable(children_peak: Option<u64>, floor: Option<u64>) -> Option<u64> {
    let peak = children_peak?;
    match floor {
        Some(floor) if peak <= floor => None,
        _ => Some(peak),
    }
}

#[cfg(unix)]
fn children_peak_rss() -> Option<u64> {
    use nix::sys::resource::{UsageWho, getrusage};
    let raw = u64::try_from(getrusage(UsageWho::RUSAGE_CHILDREN).ok()?.max_rss()).ok()?;
    // Darwin reports ru_maxrss in bytes, Linux and the BSDs in KiB.
    Some(if cfg!(target_vendor = "apple") {
        raw
    } else {
        raw * 1024
    })
}

#[cfg(not(unix))]
fn children_peak_rss() -> Option<u64> {
    None
}

/// This process's memory high-water mark (`VmHWM`), which is what a
/// vfork-spawned child starts its `maxrss` from on Linux.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn inherited_floor() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmHWM:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

/// Elsewhere a spawned child starts with a fresh address space and counter.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn inherited_floor() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::attributable;

    #[test]
    fn a_peak_at_or_below_the_inherited_floor_is_not_measurable() {
        assert_eq!(attributable(Some(5_000), Some(5_000)), None);
        assert_eq!(attributable(Some(4_000), Some(5_000)), None);
    }

    #[test]
    fn a_peak_above_the_floor_belongs_to_the_measured_binary() {
        assert_eq!(attributable(Some(6_000), Some(5_000)), Some(6_000));
    }

    #[test]
    fn without_a_floor_the_reported_peak_is_used_as_is() {
        assert_eq!(attributable(Some(6_000), None), Some(6_000));
        assert_eq!(attributable(None, None), None);
    }
}
