//! Panic isolation and crash reports (spec: workspace-architecture, Panic
//! isolation and crash reports).
//!
//! A panic inside a session, tool, adapter, plugin host or RPC handler task
//! is caught at the task boundary ([`isolate`], [`spawn_isolated`]), written
//! as a redacted crash report to `<profile home>/logs/crash/<UTC
//! timestamp>-<task>.txt` and turned into a `task.panicked` error (kind
//! `internal`) for that task only. [`catch_main`] does the same for the main
//! thread, whose caller then exits 70.
//!
//! This relies on `panic = "unwind"` (the default); the release profile must
//! never switch to `abort`.

use std::any::Any;
use std::backtrace::Backtrace;
use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
use std::fs::File;
use std::future::Future;
use std::io::{self, Write as _};
use std::panic::{self, AssertUnwindSafe, PanicHookInfo};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Once};
use std::task::{Context, Poll};

use tokio::task::JoinHandle;

use crate::error::{Error, codes};
use crate::private_fs;
use crate::secret::Redactor;

thread_local! {
    /// Location and backtrace of the last panic on this thread, captured by
    /// the hook because the unwind payload carries only the message.
    static LAST_PANIC: RefCell<Option<(String, String)>> = const { RefCell::new(None) };
    /// Depth of isolation boundaries on this thread; inside one, the default
    /// hook's stderr dump is suppressed (it would corrupt a TUI or `--json`).
    static ISOLATED: Cell<u32> = const { Cell::new(0) };
}

static HOOK: Once = Once::new();

/// Installs the process panic hook once. Panics outside an isolation
/// boundary still reach the previously installed hook.
pub fn install_hook() {
    HOOK.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info: &PanicHookInfo<'_>| {
            let location = info
                .location()
                .map_or_else(|| "unknown".to_owned(), ToString::to_string);
            let backtrace = Backtrace::force_capture().to_string();
            LAST_PANIC.with(|last| *last.borrow_mut() = Some((location, backtrace)));
            if ISOLATED.with(Cell::get) == 0 {
                previous(info);
            }
        }));
    });
}

/// Writes crash reports into one directory.
#[derive(Debug, Clone)]
pub struct CrashReporter {
    dir: PathBuf,
    redactor: Redactor,
}

impl CrashReporter {
    /// Reports go to `dir`; every value `redactor` knows is removed from them.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>, redactor: Redactor) -> Self {
        Self {
            dir: dir.into(),
            redactor,
        }
    }

    /// Reports for a profile (or the home root): `<home>/logs/crash/`.
    #[must_use]
    pub fn for_home(home: &Path, redactor: Redactor) -> Self {
        Self::new(home.join("logs").join("crash"), redactor)
    }

    /// The directory reports are written to.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn write(&self, task: &str, panic: &Caught) -> io::Result<PathBuf> {
        private_fs::create_dir(&self.dir)?;
        let now = jiff::Timestamp::now();
        let stem = format!("{}-{}", now.strftime("%Y%m%dT%H%M%S%.3fZ"), file_safe(task));
        let mut body = String::new();
        let _ = writeln!(body, "Prometheus crash report");
        let _ = writeln!(body, "version: {}", env!("CARGO_PKG_VERSION"));
        let _ = writeln!(body, "time: {now}");
        let _ = writeln!(body, "task: {task}");
        let _ = writeln!(body, "message: {}", panic.message);
        let _ = writeln!(body, "location: {}", panic.location);
        let _ = writeln!(body, "\nbacktrace:\n{}", panic.backtrace);
        let body = self.redactor.redact(&body);

        let (path, mut file) = create_unique(&self.dir, &stem)?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
        Ok(path)
    }

    /// Writes the report and builds the error the caller returns.
    fn report(&self, task: &str, panic: &Caught) -> Error {
        let error = Error::new(&codes::TASK_PANICKED, format!("task '{task}' panicked"));
        match self.write(task, panic) {
            Ok(path) => error.with_hint(format!("crash report: {}", path.display())),
            Err(io) => error
                .with_hint(format!(
                    "the crash report could not be written to {}",
                    self.dir.display()
                ))
                .with_source(io),
        }
    }
}

/// What the boundary learned about one panic.
struct Caught {
    message: String,
    location: String,
    backtrace: String,
}

impl Caught {
    fn from_payload(payload: &(dyn Any + Send)) -> Self {
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".to_owned());
        let (location, backtrace) = LAST_PANIC
            .with(|last| last.borrow_mut().take())
            .unwrap_or_else(|| ("unknown".to_owned(), "not captured".to_owned()));
        Self {
            message,
            location,
            backtrace,
        }
    }
}

/// Runs `f` inside an isolation boundary on this thread.
fn catch<R>(f: impl FnOnce() -> R) -> Result<R, Box<dyn Any + Send>> {
    ISOLATED.with(|depth| depth.set(depth.get() + 1));
    let outcome = panic::catch_unwind(AssertUnwindSafe(f));
    ISOLATED.with(|depth| depth.set(depth.get() - 1));
    outcome
}

/// Polls the inner future inside an isolation boundary on every poll, so a
/// panic on whichever worker thread polls it is caught there.
struct CatchUnwind<F> {
    inner: Pin<Box<F>>,
}

impl<F: Future> Future for CatchUnwind<F> {
    type Output = Result<F::Output, Box<dyn Any + Send>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = self.get_mut().inner.as_mut();
        match catch(|| inner.poll(cx)) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(output)) => Poll::Ready(Ok(output)),
            Err(payload) => Poll::Ready(Err(payload)),
        }
    }
}

/// Runs `work` as task `task`; a panic becomes a crash report plus a
/// `task.panicked` error instead of unwinding into the caller.
///
/// # Errors
///
/// `task.panicked` (kind `internal`) when `work` panicked.
pub async fn isolate<F: Future>(
    task: &str,
    reporter: &CrashReporter,
    work: F,
) -> Result<F::Output, Error> {
    install_hook();
    let caught = CatchUnwind {
        inner: Box::pin(work),
    }
    .await;
    caught.map_err(|payload| reporter.report(task, &Caught::from_payload(payload.as_ref())))
}

/// Spawns `work` on the tokio runtime behind [`isolate`].
pub fn spawn_isolated<F>(
    task: impl Into<String>,
    reporter: Arc<CrashReporter>,
    work: F,
) -> JoinHandle<Result<F::Output, Error>>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let task = task.into();
    tokio::spawn(async move { isolate(&task, &reporter, work).await })
}

/// Runs the main-thread body `f`, which returns the process exit status.
///
/// # Errors
///
/// `task.panicked` when `f` panicked, after the crash report (task `main`)
/// is written; the caller prints it and exits with its exit code, 70.
pub fn catch_main(reporter: &CrashReporter, f: impl FnOnce() -> u8) -> Result<u8, Error> {
    install_hook();
    catch(f).map_err(|payload| reporter.report("main", &Caught::from_payload(payload.as_ref())))
}

/// Task names become part of a file name: keep it portable and short.
fn file_safe(task: &str) -> String {
    let safe: String = task
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if safe.is_empty() {
        "task".to_owned()
    } else {
        safe
    }
}

/// `<stem>.txt`, or `<stem>-<n>.txt` when two panics share a millisecond.
fn create_unique(dir: &Path, stem: &str) -> io::Result<(PathBuf, File)> {
    let mut attempt = 0u32;
    loop {
        let name = if attempt == 0 {
            format!("{stem}.txt")
        } else {
            format!("{stem}-{attempt}.txt")
        };
        let path = dir.join(name);
        match private_fs::new_file_options().open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempt < 1000 => attempt += 1,
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::{CrashReporter, catch_main, file_safe, isolate, spawn_isolated};
    use crate::error::ErrorKind;
    use crate::secret::{Redactor, SecretString};

    fn reports(reporter: &CrashReporter) -> Vec<String> {
        let mut found: Vec<String> = std::fs::read_dir(reporter.dir())
            .map(|dir| {
                dir.map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap())
                    .collect()
            })
            .unwrap_or_default();
        found.sort();
        found
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn panicking_tool_task_does_not_affect_other_tasks() {
        let home = tempfile::tempdir().unwrap();
        let reporter = Arc::new(CrashReporter::for_home(home.path(), Redactor::default()));

        let healthy = spawn_isolated("session:a", reporter.clone(), async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            "still serving"
        });
        let broken = spawn_isolated("tool:terminal", reporter.clone(), async {
            panic!("tool exploded");
        });

        let err = broken.await.expect("task joined").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Internal);
        assert_eq!(err.code().code, "task.panicked");
        assert_eq!(err.message(), "task 'tool:terminal' panicked");
        assert_eq!(healthy.await.unwrap().unwrap(), "still serving");

        let entries: Vec<_> = std::fs::read_dir(home.path().join("logs/crash"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].ends_with("Z-tool_terminal.txt"), "{entries:?}");
        let report = &reports(&reporter)[0];
        assert!(report.contains("task: tool:terminal"));
        assert!(report.contains("message: tool exploded"));
        assert!(report.contains("location: crates/pmt-core/src/panic.rs:"));
        assert!(err.hint().unwrap().starts_with("crash report: "));
    }

    #[tokio::test]
    async fn crash_report_is_redacted() {
        let home = tempfile::tempdir().unwrap();
        let secret = SecretString::new("sk-live-0123456789");
        let reporter = CrashReporter::for_home(home.path(), Redactor::new([&secret]));
        let outcome = isolate("provider:openai", &reporter, async {
            panic!("bad response for key sk-live-0123456789");
        })
        .await;
        assert!(outcome.is_err());
        let report = &reports(&reporter)[0];
        assert!(!report.contains("sk-live-0123456789"));
        assert!(report.contains("bad response for key [REDACTED]"));
    }

    #[tokio::test]
    async fn completed_work_passes_through() {
        let home = tempfile::tempdir().unwrap();
        let reporter = CrashReporter::for_home(home.path(), Redactor::default());
        assert_eq!(isolate("rpc", &reporter, async { 5 }).await.unwrap(), 5);
        assert!(reports(&reporter).is_empty());
    }

    #[test]
    fn main_thread_panic_writes_report_and_exits_70() {
        let home = tempfile::tempdir().unwrap();
        let reporter = CrashReporter::for_home(home.path(), Redactor::default());
        let err = catch_main(&reporter, || panic!("boom")).unwrap_err();
        assert_eq!(err.exit_code(), 70);
        assert!(reports(&reporter)[0].contains("task: main"));
        assert_eq!(catch_main(&reporter, || 0).unwrap(), 0);
    }

    #[test]
    fn two_panics_in_one_millisecond_get_distinct_files() {
        let home = tempfile::tempdir().unwrap();
        let reporter = CrashReporter::for_home(home.path(), Redactor::default());
        for _ in 0..3 {
            let _ = catch_main(&reporter, || panic!("again"));
        }
        assert_eq!(reports(&reporter).len(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn crash_reports_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let reporter = CrashReporter::for_home(home.path(), Redactor::default());
        let _ = catch_main(&reporter, || panic!("perm"));
        let dir_mode = std::fs::metadata(reporter.dir())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700);
        let file = std::fs::read_dir(reporter.dir())
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn task_names_are_file_safe() {
        assert_eq!(file_safe("adapter:telegram/1"), "adapter_telegram_1");
        assert_eq!(file_safe(""), "task");
        assert_eq!(file_safe(&"x".repeat(100)).len(), 64);
    }
}
